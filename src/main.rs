use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};

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
    /// Top senders by message count (omit --mailbox to scan all mailboxes)
    TopSenders {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long, default_value = "0")]
        offset: usize,
        #[arg(long, default_value = "10")]
        limit: usize,
    },
    /// Top exact Header From domains and subdomains (omit --mailbox to scan all)
    TopDomains {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long, default_value = "0")]
        offset: usize,
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Top bulk-mail senders by List-Unsubscribe-Post presence, then count (omit --mailbox to scan all)
    TopSubscriptions {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long, default_value = "0")]
        offset: usize,
        #[arg(long, default_value = "10")]
        limit: usize,
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
        #[arg(long)]
        expected_uid_validity: u32,
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
        #[arg(long)]
        expected_uid_validity: u32,
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
        #[arg(long)]
        expected_uid_validity: u32,
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
    /// List durable COPY-fallback MOVE operations awaiting recovery or review
    ListPendingMoves {
        #[arg(long)]
        account: String,
    },
    /// Safely reconcile one or all pending COPY-fallback MOVE operations
    ReconcileMoves {
        #[arg(long)]
        account: String,
        /// Operation ID from list-pending-moves; omit to reconcile all
        #[arg(long)]
        operation_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing — logs to stderr so MCP JSON-RPC on stdout is unaffected.
    // ANSI only when stderr is a terminal: MCP hosts capture stderr to log files.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    agentmail::secret::init_service_name("agentmail");
    init_platform_keyring();
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

            let password = prompt_secret(&format!(
                "Enter password for {} ({}): ",
                account, acct_config.username
            ))?;

            agentmail::credentials::set_password(&account, acct_config, &password).await?;
            eprintln!("Password stored successfully.");
            Ok(())
        }
        CliCommand::Configure { provider } => configure_account(provider.as_deref()).await,
        CliCommand::ListFlags { account, mailbox } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .list_flags(mailbox.as_deref(), &account, None, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::DownloadAttachments {
            account,
            mailbox,
            uid,
            expected_uid_validity,
            output_dir,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .download_attachments(
                    &mailbox,
                    &account,
                    uid,
                    expected_uid_validity,
                    std::path::Path::new(&output_dir),
                )
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
                .find_attachments(mailbox.as_deref(), &account, offset, limit, None, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::TopSenders {
            account,
            mailbox,
            offset,
            limit,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .top_senders(mailbox.as_deref(), &account, offset, limit, None, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::TopDomains {
            account,
            mailbox,
            offset,
            limit,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .top_domains(mailbox.as_deref(), &account, offset, limit, None, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::TopSubscriptions {
            account,
            mailbox,
            offset,
            limit,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .top_subscriptions(mailbox.as_deref(), &account, offset, limit, None, None)
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
            expected_uid_validity,
            include_content,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .get_messages_by_uid(
                    &mailbox,
                    &account,
                    &uids,
                    expected_uid_validity,
                    include_content,
                    false,
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::AddFlags {
            account,
            mailbox,
            uid,
            expected_uid_validity,
            flags,
            color,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .add_flags(
                    &mailbox,
                    &account,
                    uid,
                    expected_uid_validity,
                    &flags,
                    color.as_deref(),
                )
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
                .create_draft(&account, &subject, &body, &to, &cc, &bcc, &[])
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::ListPendingMoves { account } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk.list_pending_moves(&account).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::ReconcileMoves {
            account,
            operation_id,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .reconcile_moves(&account, operation_id.as_deref(), None, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Platform keyring initialization
// ---------------------------------------------------------------------------

/// Install the platform-appropriate keyring store as the default for `keyring_core`.
///
/// On macOS, prefers the data-protection keychain (which doesn't depend on a default
/// keychain pointer and works in headless launchd contexts). Probes it with a no-op
/// read; if the binary lacks the entitlement (typical for unsigned `cargo install`
/// builds), falls back to the legacy file-based keychain. Surfaces failures via
/// `tracing` so the cause is visible instead of silently swallowed.
fn init_platform_keyring() {
    #[cfg(target_os = "macos")]
    {
        let protected = apple_native_keyring_store::protected::Store::new()
            .expect("protected::Store::new is documented infallible");
        keyring_core::set_default_store(protected);

        // Probe: try to read a sentinel entry. NoEntry means the store works
        // (entry just doesn't exist). MissingEntitlement / PlatformFailure means
        // we need to fall back to the file-based keychain.
        let probe = keyring_core::Entry::new("agentmail.probe.startup", "probe");
        let probe_ok = match probe {
            Ok(entry) => match entry.get_password() {
                Ok(_) => true,
                Err(keyring_core::error::Error::NoEntry) => true,
                Err(e) => {
                    let msg = e.to_string();
                    let entitlement_issue = msg.contains("-34018")
                        || msg.to_lowercase().contains("missing entitlement");
                    if entitlement_issue {
                        tracing::debug!(
                            "keychain: data-protection unavailable (missing entitlement); \
                             falling back to file-based keychain"
                        );
                        false
                    } else {
                        // Some other error — keep data-protection, surface details
                        // on the next real call.
                        tracing::debug!(
                            "keychain: data-protection probe returned {e}; keeping it active"
                        );
                        true
                    }
                }
            },
            Err(e) => {
                tracing::warn!("keychain: Entry probe failed unexpectedly: {e}");
                false
            }
        };

        if !probe_ok {
            match apple_native_keyring_store::keychain::Store::new() {
                Ok(store) => {
                    keyring_core::set_default_store(store);
                    tracing::debug!("keychain: using file-based backend");
                }
                Err(e) => {
                    tracing::warn!(
                        "keychain: no backend available (file-based failed: {e}). \
                         Password operations will fail. \
                         Set AGENTMAIL_PASSWORD_<ACCOUNT> to bypass the keychain."
                    );
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        match windows_native_keyring_store::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(e) => tracing::warn!(
                "keychain: Windows credential store unavailable: {e}. \
                 Set AGENTMAIL_PASSWORD_<ACCOUNT> to bypass."
            ),
        }
    }

    #[cfg(target_os = "linux")]
    {
        match dbus_secret_service_keyring_store::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(e) => tracing::warn!(
                "keychain: D-Bus secret service unavailable: {e}. \
                 Set AGENTMAIL_PASSWORD_<ACCOUNT> to bypass."
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Interactive account configuration
// ---------------------------------------------------------------------------

fn prompt(label: &str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn prompt_default(label: &str, default: &str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{} [{}]: ", label, default);
    std::io::stderr().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let val = buf.trim();
    if val.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(val.to_string())
    }
}

fn trim_line_ending(mut value: String) -> String {
    while value.ends_with('\r') || value.ends_with('\n') {
        value.pop();
    }
    value
}

#[cfg(unix)]
struct TerminalEchoGuard;

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn prompt_secret(label: &str) -> Result<String, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    if std::io::stdin().is_terminal() {
        let script = r#"
            $secret = Read-Host -Prompt $env:AGENTMAIL_SECRET_PROMPT -AsSecureString
            $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secret)
            try {
                [Console]::Out.WriteLine([Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer))
            } finally {
                [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer)
            }
        "#;
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-Command", script])
            .env("AGENTMAIL_SECRET_PROMPT", label.trim_end())
            .stdin(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .output()?;
        if !output.status.success() {
            return Err(format!("hidden password prompt failed: {}", output.status).into());
        }
        return Ok(trim_line_ending(
            String::from_utf8(output.stdout)
                .map_err(|_| "hidden password prompt returned invalid UTF-8")?,
        ));
    }

    eprint!("{label}");
    std::io::stderr().flush()?;

    #[cfg(unix)]
    let echo_guard = if std::io::stdin().is_terminal() {
        let status = std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err("could not disable terminal echo for the password prompt".into());
        }
        Some(TerminalEchoGuard)
    } else {
        None
    };

    let mut value = String::new();
    let read_result = std::io::stdin().read_line(&mut value);
    #[cfg(unix)]
    if echo_guard.is_some() {
        drop(echo_guard);
        eprintln!();
    }
    read_result?;
    Ok(trim_line_ending(value))
}

fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

struct TemporaryConfigFile {
    path: std::path::PathBuf,
    armed: bool,
}

impl Drop for TemporaryConfigFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_private_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    #[cfg(unix)]
    let parent_existed = parent.exists();
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    if !parent_existed {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary_path = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
    let mut temporary = TemporaryConfigFile {
        path: temporary_path.clone(),
        armed: true,
    };

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary_path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&temporary_path, path)?;
    temporary.armed = false;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
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
    if !valid_account_name(&account_name) {
        return Err("Account name may contain only ASCII letters, numbers, '-' and '_'".into());
    }

    // 3. Host / port / username
    let (host, port, username) = if let Some(ref p) = preset {
        let host = prompt_default("IMAP host", p.host)?;
        let port_str = prompt_default("IMAP port", &p.port.to_string())?;
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("Invalid IMAP port '{port_str}'"))?;
        eprintln!("  (hint: {})", p.username_hint);
        let username = prompt("Username: ")?;
        (host, port, username)
    } else {
        let host = prompt("IMAP host: ")?;
        let port_str = prompt_default("IMAP port", "993")?;
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("Invalid IMAP port '{port_str}'"))?;
        let username = prompt("Username: ")?;
        (host, port, username)
    };
    if host.is_empty() || username.is_empty() {
        return Err("Host and username are required".into());
    }

    let suggested_email = agentmail::config::canonicalize_email(&username).unwrap_or_default();
    let email = if suggested_email.is_empty() {
        prompt("Primary email (optional, used to recognize your own mail): ")?
    } else {
        prompt_default("Primary email", &suggested_email)?
    };
    let aliases = prompt("Aliases (comma-separated, optional): ")?
        .split(',')
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    let mut account_config = agentmail::AccountConfig::new(&host, &username)
        .with_port(port)
        .with_aliases(aliases);
    if !email.is_empty() {
        account_config = account_config.with_email(email);
    }
    let validated =
        agentmail::Config::try_from_accounts(vec![(account_name.clone(), account_config)])?;
    let account_config = validated
        .accounts
        .get(&account_name)
        .expect("validated account is present");
    let host = account_config.host.clone();
    let username = account_config.username.clone();
    let email = account_config.email.clone();
    let aliases = account_config.aliases.clone();

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
                shell_quote(&host),
                shell_quote(&username)
            );
            eprintln!("  (hint: use the default to read Apple Mail's stored password)");
            let cmd = prompt_default("Command", &default_cmd)?;
            (format!("password.cmd = {:?}", cmd), false)
        }
        "raw" | "3" => {
            let pw = prompt_secret("Password: ")?;
            (format!("password.raw = {:?}", pw), false)
        }
        "keyring" | "1" => {
            // keyring (default)
            (
                format!("password.keyring = {:?}", format!("mail.{username}")),
                true,
            )
        }
        _ => return Err("Password method must be keyring, command, or raw".into()),
    };

    // 5. Write config file
    let config_path = agentmail::Config::default_path();
    let existing = if config_path.exists() {
        let config = agentmail::Config::load_from(&config_path)?;
        if config.accounts.contains_key(&account_name) {
            return Err(format!(
                "Account '{}' already exists in {}. Edit the file directly to modify it.",
                account_name,
                config_path.display()
            )
            .into());
        }
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    let mut section = format!("[accounts.{account_name:?}]\n");
    section.push_str(&format!("host = {:?}\n", host));
    if port != 993 {
        section.push_str(&format!("port = {}\n", port));
    }
    section.push_str(&format!("username = {:?}\n", username));
    if let Some(email) = email {
        section.push_str(&format!("email = {email:?}\n"));
    }
    if !aliases.is_empty() {
        section.push_str(&format!("aliases = {aliases:?}\n"));
    }
    section.push_str(&format!("{}\n", password_toml));

    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let combined = format!("{existing}{separator}\n{section}");
    let mut parsed: agentmail::Config = toml::from_str(&combined)
        .map_err(|error| format!("Generated config is invalid: {}", error.message()))?;
    parsed.normalize_and_validate()?;
    write_private_atomic(&config_path, combined.as_bytes())?;
    eprintln!(
        "\nWrote account '{}' to {}",
        account_name,
        config_path.display()
    );

    // 6. Store password in keyring if needed
    if need_store_password {
        let pw = prompt_secret(&format!(
            "Enter password for {} ({}): ",
            account_name, username
        ))?;

        let mut secret = agentmail::secret::Secret::new_keyring(format!("mail.{}", username));
        secret
            .set(&pw)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_reject_toml_structure_characters() {
        assert!(valid_account_name("work_2026-07"));
        assert!(!valid_account_name("work]\n[accounts.evil"));
    }

    #[test]
    fn top_domains_cli_uses_the_documented_page_defaults() {
        let cli = Cli::try_parse_from(["agentmail", "top-domains", "--account", "work"])
            .expect("valid command");

        match cli.command.expect("subcommand") {
            CliCommand::TopDomains {
                account,
                mailbox,
                offset,
                limit,
            } => {
                assert_eq!(account, "work");
                assert_eq!(mailbox, None);
                assert_eq!(offset, 0);
                assert_eq!(limit, 20);
            }
            _ => panic!("expected top-domains"),
        }
    }

    #[test]
    fn top_domains_cli_preserves_explicit_offset_and_limit() {
        let cli = Cli::try_parse_from([
            "agentmail",
            "top-domains",
            "--account",
            "work",
            "--offset",
            "7",
            "--limit",
            "7",
        ])
        .expect("valid command");

        match cli.command.expect("subcommand") {
            CliCommand::TopDomains { offset, limit, .. } => {
                assert_eq!(offset, 7);
                assert_eq!(limit, 7);
            }
            _ => panic!("expected top-domains"),
        }
    }

    #[test]
    fn reconcile_moves_cli_accepts_one_operation_id() {
        let cli = Cli::try_parse_from([
            "agentmail",
            "reconcile-moves",
            "--account",
            "work",
            "--operation-id",
            "operation-123",
        ])
        .expect("valid command");

        match cli.command.expect("subcommand") {
            CliCommand::ReconcileMoves {
                account,
                operation_id,
            } => {
                assert_eq!(account, "work");
                assert_eq!(operation_id.as_deref(), Some("operation-123"));
            }
            _ => panic!("expected reconcile-moves"),
        }
    }

    #[test]
    fn private_atomic_write_replaces_complete_file() {
        let directory =
            std::env::temp_dir().join(format!("agentmail-config-write-{}", uuid::Uuid::new_v4()));
        let path = directory.join("config.toml");

        write_private_atomic(&path, b"first").expect("first write");
        write_private_atomic(&path, b"second").expect("replacement write");
        let contents = std::fs::read(&path).expect("read replacement");
        let _ = std::fs::remove_dir_all(&directory);

        assert_eq!(contents, b"second");
    }

    #[cfg(unix)]
    #[test]
    fn private_atomic_write_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory =
            std::env::temp_dir().join(format!("agentmail-config-mode-{}", uuid::Uuid::new_v4()));
        let path = directory.join("config.toml");
        write_private_atomic(&path, b"secret").expect("private write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        let _ = std::fs::remove_dir_all(&directory);

        assert_eq!(mode, 0o600);
    }
}
