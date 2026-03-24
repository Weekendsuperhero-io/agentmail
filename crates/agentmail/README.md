# agentmail

A Rust IMAP client library for email management. Read, search, delete, unsubscribe, and manage mailboxes across Gmail, iCloud, Outlook, Yahoo, Fastmail, or any IMAP server.

[![Crates.io](https://img.shields.io/crates/v/agentmail.svg)](https://crates.io/crates/agentmail)
[![Documentation](https://docs.rs/agentmail/badge.svg)](https://docs.rs/agentmail)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

## Features

- Full IMAP client with connection pooling (configurable per-account)
- Message fetching, searching, and pagination
- Bulk delete by sender, by List-Id, or by UID
- Mailing list ranking by sender or List-Id (RFC 2919)
- RFC 8058 one-click unsubscribe
- Apple Mail color flag support ($MailFlagBit0-2)
- Attachment detection and download
- Draft composition via RFC 822
- Credential storage via OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret Service)
- Provider presets for Gmail, iCloud, Outlook, Yahoo, Fastmail

## Usage

```rust
use agentmail::{Agentmail, Config};

let config = Config::from_accounts(vec![
    ("gmail".to_string(),
     agentmail::AccountConfig::new("imap.gmail.com", "user@gmail.com")),
]);
let mail = Agentmail::new(config);

// List mailboxes
let mailboxes = mail.list_mailboxes(None).await?;

// Rank senders by volume
let ranked = mail.group_by_sender(None, "gmail", Some(10), None).await?;

// Search messages
let results = mail.search_messages(
    "INBOX", "gmail", &criteria, 0, 25, false, false
).await?;
```

## MCP Server

For AI agent integration via MCP (Model Context Protocol), see [`agentmail-app`](https://crates.io/crates/agentmail-app).

## License

Apache-2.0
