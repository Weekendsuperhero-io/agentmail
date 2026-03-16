pub mod config;
pub mod connection;
pub mod content;
pub mod credentials;
pub mod draft;
pub mod error;
pub mod imap_client;
pub mod parser;
pub mod types;

pub use config::Config;
pub use connection::ConnectionPool;
pub use error::{MailkitError, Result};
pub use imap_client::ProgressFn;
pub use types::*;

/// High-level facade for IMAP operations.
/// Owns the connection pool and configuration.
pub struct Mailkit {
    pool: ConnectionPool,
}

impl Mailkit {
    /// Create from an existing config.
    pub fn new(config: Config) -> Self {
        Self {
            pool: ConnectionPool::new(config),
        }
    }

    /// Load config from the default path and create.
    pub fn from_default_config() -> Result<Self> {
        let config = Config::load()?;
        Ok(Self::new(config))
    }

    /// List all configured account names.
    pub fn account_names(&self) -> Vec<String> {
        self.pool.account_names()
    }

    /// Get config for a specific account.
    pub fn account_config(&self, name: &str) -> Option<&config::AccountConfig> {
        self.pool.account_config(name)
    }

    /// Get the underlying config.
    pub fn config(&self) -> &Config {
        self.pool.config()
    }

    // -----------------------------------------------------------------
    // Account & connection
    // -----------------------------------------------------------------

    /// List accounts as JSON.
    pub async fn list_accounts(&self) -> Result<serde_json::Value> {
        let config = self.pool.config();
        let default = config.default_account();

        let mut accounts: Vec<AccountInfo> = config
            .accounts
            .iter()
            .map(|(name, cfg)| AccountInfo {
                name: name.clone(),
                host: cfg.host.clone(),
                username: cfg.username.clone(),
                is_default: default == Some(name.as_str()),
            })
            .collect();
        accounts.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(serde_json::json!({ "accounts": accounts }))
    }

    /// Check IMAP connectivity for an account.
    pub async fn check_connection(&self, account: &str) -> Result<ConnectionStatus> {
        match self.pool.acquire(account).await {
            Ok(session) => {
                session.release().await;
                Ok(ConnectionStatus {
                    account: account.to_string(),
                    connected: true,
                    error: None,
                    server_greeting: None,
                })
            }
            Err(e) => Ok(ConnectionStatus {
                account: account.to_string(),
                connected: false,
                error: Some(e.to_string()),
                server_greeting: None,
            }),
        }
    }

    /// List IMAP capabilities for an account.
    pub async fn list_capabilities(&self, account: &str) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        let caps = imap_client::list_capabilities(session.session()).await?;
        session.release().await;

        Ok(serde_json::json!({
            "account": account,
            "capabilities": caps,
        }))
    }

    // -----------------------------------------------------------------
    // Mailboxes
    // -----------------------------------------------------------------

    /// List mailboxes, optionally scoped to an account.
    pub async fn list_mailboxes(
        &self,
        account: Option<&str>,
    ) -> Result<serde_json::Value> {
        let account_names: Vec<String> = if let Some(name) = account {
            if !self.pool.config().accounts.contains_key(name) {
                return Err(MailkitError::AccountNotFound(name.to_string()));
            }
            vec![name.to_string()]
        } else {
            self.pool.account_names()
        };

        let mut all_mailboxes: Vec<MailboxInfo> = Vec::new();
        for acct_name in &account_names {
            let mut session = self.pool.acquire(acct_name).await?;
            let mailboxes =
                imap_client::list_mailboxes(session.session(), acct_name).await?;
            session.release().await;
            all_mailboxes.extend(mailboxes);
        }

        Ok(serde_json::json!({ "mailboxes": all_mailboxes }))
    }

    /// Create a new mailbox on the server.
    pub async fn create_mailbox(
        &self,
        account: &str,
        mailbox_name: &str,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::create_mailbox(session.session(), mailbox_name).await?;
        imap_client::sync(session.session()).await?;
        session.release().await;

        Ok(serde_json::json!({
            "account": account,
            "mailbox": mailbox_name,
            "created": true,
        }))
    }

    // -----------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------

    /// Fetch messages with pagination (newest first).
    pub async fn get_messages(
        &self,
        mailbox: &str,
        account: &str,
        offset: usize,
        limit: usize,
        include_content: bool,
        include_headers: bool,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        let (messages, total) = imap_client::fetch_messages(
            session.session(),
            mailbox,
            account,
            offset,
            limit,
            include_content,
            include_headers,
        )
        .await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "offset": offset,
            "limit": limit,
            "total": total,
            "messages": messages,
        }))
    }

    /// Fetch specific messages by UID.
    pub async fn get_messages_by_uid(
        &self,
        mailbox: &str,
        account: &str,
        uids: &[u32],
        include_content: bool,
        include_headers: bool,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;
        let messages =
            imap_client::fetch_by_uids(session.session(), uids, mailbox, account, include_content, include_headers)
                .await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "messages": messages,
        }))
    }

    /// Search messages using IMAP criteria.
    pub async fn search_messages(
        &self,
        mailbox: &str,
        account: &str,
        criteria: &SearchCriteria,
        offset: usize,
        limit: usize,
        include_content: bool,
        include_headers: bool,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        let (messages, total) = imap_client::search_messages(
            session.session(),
            mailbox,
            account,
            criteria,
            offset,
            limit,
            include_content,
            include_headers,
        )
        .await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "offset": offset,
            "limit": limit,
            "totalMatches": total,
            "messages": messages,
        }))
    }

    // -----------------------------------------------------------------
    // Group by sender
    // -----------------------------------------------------------------

    /// Group messages by sender address with counts and date ranges.
    /// Sorted by message count descending.
    ///
    /// When `mailbox` is `None`, scans all mailboxes in the account.
    pub async fn group_by_sender(
        &self,
        mailbox: Option<&str>,
        account: &str,
        limit: Option<usize>,
        on_progress: Option<&ProgressFn>,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => imap_client::list_mailbox_names(session.session()).await?,
        };

        use std::collections::HashMap;
        let mut map: HashMap<String, SenderSummary> = HashMap::new();

        for mbox in &mailboxes {
            let sender_dates = match imap_client::fetch_sender_dates(session.session(), mbox, on_progress).await {
                Ok(data) => data,
                Err(_) => continue, // skip unselectable mailboxes
            };

            for (email, display_name, date) in sender_dates {
                if email.is_empty() {
                    continue;
                }
                let entry = map.entry(email.clone()).or_insert_with(|| SenderSummary {
                    sender: String::new(),
                    address: email,
                    display_name: display_name.clone(),
                    count: 0,
                    oldest_date: None,
                    newest_date: None,
                });

                entry.count += 1;

                if !display_name.is_empty() {
                    entry.display_name = display_name;
                }

                if let Some(d) = date {
                    entry.oldest_date = Some(match entry.oldest_date {
                        Some(existing) => existing.min(d),
                        None => d,
                    });
                    entry.newest_date = Some(match entry.newest_date {
                        Some(existing) => existing.max(d),
                        None => d,
                    });
                }
            }
        }

        session.release().await;

        let mut senders: Vec<SenderSummary> = map.into_values().collect();
        for s in &mut senders {
            s.sender = if s.display_name.is_empty() {
                s.address.clone()
            } else {
                format!("{} <{}>", s.display_name, s.address)
            };
        }
        senders.sort_by(|a, b| b.count.cmp(&a.count));

        let unique_senders = senders.len();
        if let Some(n) = limit {
            senders.truncate(n);
        }

        let mailbox_label = mailbox.unwrap_or("*");
        Ok(serde_json::json!({
            "mailbox": mailbox_label,
            "account": account,
            "totalMessages": senders.iter().map(|s| s.count).sum::<u32>(),
            "uniqueSenders": unique_senders,
            "senders": senders,
        }))
    }

    /// Group mailing-list messages by sender.
    ///
    /// Includes messages that have List-Unsubscribe and/or List-Id.
    /// Groups by exact sender (email + display name). The sample_uid and
    /// unsubscribe info come from the newest message in each group.
    ///
    /// When `mailbox` is `None`, scans all mailboxes in the account.
    pub async fn group_by_list(
        &self,
        mailbox: Option<&str>,
        account: &str,
        limit: Option<usize>,
        on_progress: Option<&ProgressFn>,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => imap_client::list_mailbox_names(session.session()).await?,
        };

        use std::collections::HashMap;
        use types::ListSummary;

        // Key by (email, display_name) for exact sender grouping
        let mut map: HashMap<(String, String), ListSummary> = HashMap::new();

        for mbox in &mailboxes {
            let rows = match imap_client::fetch_list_headers(session.session(), mbox, on_progress).await {
                Ok(data) => data,
                Err(_) => continue, // skip unselectable mailboxes
            };

            for row in rows {
                let key = (row.sender_email.clone(), row.sender_name.clone());
                let entry = map.entry(key).or_insert_with(|| {
                    let sender_display = if row.sender_name.is_empty() {
                        row.sender_email.clone()
                    } else {
                        format!("{} <{}>", row.sender_name, row.sender_email)
                    };
                    ListSummary {
                        sender: sender_display,
                        address: row.sender_email.clone(),
                        display_name: row.sender_name.clone(),
                        list_unsubscribe: None,
                        unsubscribe_url: None,
                        list_unsubscribe_post: None,
                        one_click: false,
                        list_id: None,
                        sample_uid: row.uid,
                        sample_mailbox: Some(mbox.clone()),
                        count: 0,
                        oldest_date: None,
                        newest_date: None,
                    }
                });

                entry.count += 1;

                // Capture list_id from any message in the group
                if entry.list_id.is_none() {
                    if let Some(ref lid) = row.list_id {
                        entry.list_id = Some(lid.clone());
                    }
                }

                // Track the newest message — its UID and unsubscribe info are used
                let is_newer = match row.date {
                    Some(d) => entry
                        .newest_date
                        .map(|existing| d > existing)
                        .unwrap_or(true),
                    None => entry.newest_date.is_none(),
                };

                if is_newer {
                    entry.sample_uid = row.uid;
                    entry.sample_mailbox = Some(mbox.clone());
                    entry.list_unsubscribe = row.list_unsubscribe.clone();
                    entry.unsubscribe_url = row
                        .list_unsubscribe
                        .as_deref()
                        .and_then(extract_https_unsubscribe_url);
                    entry.list_unsubscribe_post = row.list_unsubscribe_post.clone();
                    entry.one_click = row
                        .list_unsubscribe_post
                        .as_deref()
                        .map(|v| v.contains("List-Unsubscribe=One-Click"))
                        .unwrap_or(false);
                    if let Some(ref lid) = row.list_id {
                        entry.list_id = Some(lid.clone());
                    }
                    if !row.sender_name.is_empty() {
                        entry.display_name = row.sender_name.clone();
                        entry.sender =
                            format!("{} <{}>", entry.display_name, row.sender_email);
                    }
                }

                if let Some(d) = row.date {
                    entry.oldest_date = Some(match entry.oldest_date {
                        Some(existing) => existing.min(d),
                        None => d,
                    });
                    entry.newest_date = Some(match entry.newest_date {
                        Some(existing) => existing.max(d),
                        None => d,
                    });
                }
            }
        }

        session.release().await;

        let mut lists: Vec<ListSummary> = map.into_values().collect();
        // One-click senders first, then by message count
        lists.sort_by(|a, b| {
            b.one_click.cmp(&a.one_click).then_with(|| b.count.cmp(&a.count))
        });

        let unique_lists = lists.len();
        let total_messages = lists.iter().map(|l| l.count).sum::<u32>();
        if let Some(n) = limit {
            lists.truncate(n);
        }

        let mailbox_label = mailbox.unwrap_or("*");
        Ok(serde_json::json!({
            "mailbox": mailbox_label,
            "account": account,
            "totalMessages": total_messages,
            "uniqueLists": unique_lists,
            "lists": lists,
        }))
    }

    // -----------------------------------------------------------------
    // Flags
    // -----------------------------------------------------------------

    /// List all flags actually in use across messages in a mailbox, with counts.
    pub async fn list_flags(
        &self,
        mailbox: &str,
        account: &str,
        on_progress: Option<&ProgressFn>,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        let flags = imap_client::fetch_flags(session.session(), mailbox, on_progress).await?;
        session.release().await;

        let flag_list: Vec<serde_json::Value> = flags
            .iter()
            .map(|(name, count)| serde_json::json!({ "flag": name, "count": count }))
            .collect();

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "totalFlags": flag_list.len(),
            "flags": flag_list,
        }))
    }

    /// Set a colored flag on a message (Apple Mail / RFC draft-eggert-mailflagcolors).
    /// Uses union semantics: existing flags are preserved, only color bits + \Flagged are added/changed.
    /// Pass `color = None` to remove the flag entirely (clears \Flagged and all color bits).
    pub async fn set_flag_color(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        color: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;

        let color_bits = ["$MailFlagBit0", "$MailFlagBit1", "$MailFlagBit2"];

        match color {
            Some(color_name) => {
                let bits = color_to_bits(color_name)
                    .ok_or_else(|| MailkitError::Other(format!(
                        "Unknown flag color '{}'. Valid colors: red, orange, yellow, green, blue, purple, gray",
                        color_name
                    )))?;

                // Remove old color bits first, then add new ones + \Flagged
                imap_client::remove_flags(
                    session.session(),
                    uid,
                    &color_bits.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                ).await?;

                let mut add = vec!["\\Flagged".to_string()];
                for (i, &bit) in color_bits.iter().enumerate() {
                    if bits[i] {
                        add.push(bit.to_string());
                    }
                }
                imap_client::add_flags(session.session(), uid, &add).await?;
            }
            None => {
                // Unflag: remove \Flagged and all color bits
                let mut remove: Vec<String> = vec!["\\Flagged".to_string()];
                remove.extend(color_bits.iter().map(|s| s.to_string()));
                imap_client::remove_flags(session.session(), uid, &remove).await?;
            }
        }

        imap_client::sync(session.session()).await?;

        // Read back the updated flags
        let updated_flags = imap_client::get_flags(session.session(), uid).await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "uid": uid,
            "color": color,
            "flags": updated_flags,
        }))
    }

    /// Update flags on a message with union semantics: adds the specified flags
    /// without removing any existing flags.
    pub async fn add_flags(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        flags: &[String],
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;
        imap_client::add_flags(session.session(), uid, flags).await?;
        imap_client::sync(session.session()).await?;
        let updated_flags = imap_client::get_flags(session.session(), uid).await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "uid": uid,
            "flags": updated_flags,
        }))
    }

    /// Remove specific flags from a message.
    pub async fn remove_flags(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        flags: &[String],
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;
        imap_client::remove_flags(session.session(), uid, flags).await?;
        imap_client::sync(session.session()).await?;
        let updated_flags = imap_client::get_flags(session.session(), uid).await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "uid": uid,
            "flags": updated_flags,
        }))
    }

    // -----------------------------------------------------------------
    // Attachments
    // -----------------------------------------------------------------

    /// Find messages with attachments via Content-Type header scan.
    /// Returns UIDs and total count; use get_messages_by_uid to fetch details.
    pub async fn find_attachments(
        &self,
        mailbox: &str,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        let uids = imap_client::fetch_attachment_uids(session.session(), mailbox, on_progress).await?;
        session.release().await;

        let total = uids.len();
        let page: Vec<u32> = uids.into_iter().skip(offset).take(limit).collect();

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "total": total,
            "offset": offset,
            "limit": limit,
            "uids": page,
        }))
    }

    // -----------------------------------------------------------------
    // Delete
    // -----------------------------------------------------------------

    /// Delete one or more messages by UID.
    pub async fn delete_messages(
        &self,
        mailbox: &str,
        account: &str,
        uids: &[u32],
        on_progress: Option<&ProgressFn>,
    ) -> Result<serde_json::Value> {
        let trash = self.trash_mailbox(account);
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;
        let (deleted, failed) =
            imap_client::bulk_delete_messages(session.session(), uids, trash.as_deref(), on_progress)
                .await?;
        imap_client::sync(session.session()).await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "deleted": deleted.len(),
            "failed": failed.len(),
        }))
    }

    /// Delete all messages from an exact sender identified by UID.
    ///
    /// `mailbox` is the mailbox containing the target `uid`.
    /// When `all_mailboxes` is true, searches and deletes across every
    /// mailbox in the account (not just the source mailbox).
    pub async fn delete_by_sender(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        all_mailboxes: bool,
        on_progress: Option<&ProgressFn>,
    ) -> Result<serde_json::Value> {
        let trash = self.trash_mailbox(account);
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;

        // 1. Fetch the exact sender from the target message
        let (target_email, target_name) =
            imap_client::fetch_sender(session.session(), uid).await?;

        let sender_display = if target_name.is_empty() {
            target_email.clone()
        } else {
            format!("{} <{}>", target_name, target_email)
        };

        let search_mailboxes = if all_mailboxes {
            imap_client::list_mailbox_names(session.session()).await?
        } else {
            vec![mailbox.to_string()]
        };

        let mut total_found = 0usize;
        let mut total_deleted = 0usize;
        let mut total_failed = 0usize;
        let mut per_mailbox = Vec::new();

        for mbox in &search_mailboxes {
            if imap_client::select(session.session(), mbox).await.is_err() {
                continue;
            }

            // Server-side FROM search (substring) to get candidates
            let criteria = SearchCriteria {
                from: Some(target_email.clone()),
                deleted: Some(false),
                ..Default::default()
            };
            let query = imap_client::build_search_query_pub(&criteria);
            let candidate_uids = match imap_client::search_uids(session.session(), &query).await {
                Ok(uids) => uids,
                Err(_) => continue,
            };

            if candidate_uids.is_empty() {
                continue;
            }

            // Fetch FROM for all candidates and filter for exact match
            let candidates =
                imap_client::fetch_senders_batch(session.session(), &candidate_uids).await?;
            let exact_uids: Vec<u32> = candidates
                .into_iter()
                .filter(|(_uid, email, name)| email == &target_email && name == &target_name)
                .map(|(uid, _, _)| uid)
                .collect();

            if exact_uids.is_empty() {
                continue;
            }

            let found = exact_uids.len();
            let (deleted, failed) = imap_client::bulk_delete_messages(
                session.session(),
                &exact_uids,
                trash.as_deref(),
                on_progress,
            )
            .await?;
            imap_client::sync(session.session()).await?;

            total_found += found;
            total_deleted += deleted.len();
            total_failed += failed.len();

            if found > 0 {
                per_mailbox.push(serde_json::json!({
                    "mailbox": mbox,
                    "found": found,
                    "deleted": deleted.len(),
                    "failed": failed.len(),
                }));
            }
        }

        session.release().await;

        let mut result = serde_json::json!({
            "mailbox": if all_mailboxes { "*" } else { mailbox },
            "account": account,
            "sender": sender_display,
            "found": total_found,
            "deleted": total_deleted,
            "failed": total_failed,
        });

        if all_mailboxes {
            result["mailboxes"] = serde_json::json!(per_mailbox);
        }

        Ok(result)
    }

    // -----------------------------------------------------------------
    // Move
    // -----------------------------------------------------------------

    /// Move a message to another mailbox.
    pub async fn move_message(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        destination: &str,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;
        imap_client::move_message(session.session(), uid, destination).await?;
        imap_client::sync(session.session()).await?;
        session.release().await;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "uid": uid,
            "destination": destination,
            "moved": true,
        }))
    }

    // -----------------------------------------------------------------
    // Draft
    // -----------------------------------------------------------------

    /// Create a draft message.
    pub async fn create_draft(
        &self,
        account: &str,
        subject: &str,
        body: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
    ) -> Result<serde_json::Value> {
        let acct_config = self
            .pool
            .account_config(account)
            .ok_or_else(|| MailkitError::AccountNotFound(account.to_string()))?;
        let from = &acct_config.username;

        let rfc822 = draft::compose_draft(subject, body, to, cc, bcc, Some(from))?;

        let mut session = self.pool.acquire(account).await?;

        // Determine the drafts mailbox: explicit config, or auto-detect
        let drafts_name = if let Some(ref d) = acct_config.drafts_mailbox {
            d.clone()
        } else {
            find_drafts_mailbox(session.session()).await?.unwrap_or_else(|| "Drafts".to_string())
        };

        imap_client::append_draft(session.session(), &drafts_name, &rfc822).await?;
        imap_client::sync(session.session()).await?;
        session.release().await;

        Ok(serde_json::json!({
            "created": true,
            "account": account,
            "draftsMailbox": drafts_name,
            "subject": subject,
            "recipients": {
                "to": to,
                "cc": cc,
                "bcc": bcc,
            },
        }))
    }

    // -----------------------------------------------------------------
    // Raw source
    // -----------------------------------------------------------------

    /// Get the raw RFC822 source of a message.
    pub async fn get_message_source(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        let raw = imap_client::get_message_source(session.session(), mailbox, uid).await?;
        session.release().await;

        let source = String::from_utf8_lossy(&raw).to_string();
        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "uid": uid,
            "source": source,
        }))
    }

    // -----------------------------------------------------------------
    // Download attachments
    // -----------------------------------------------------------------

    /// Download attachments from a message to a directory.
    /// Files are named `{uid}_{original_name}`.
    pub async fn download_attachments(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        output_dir: &std::path::Path,
    ) -> Result<serde_json::Value> {
        let mut session = self.pool.acquire(account).await?;
        let raw = imap_client::get_message_source(session.session(), mailbox, uid).await?;
        session.release().await;

        // Parse attachments on a blocking thread (CPU-intensive MIME parsing)
        let attachments = tokio::task::spawn_blocking(move || {
            parser::extract_attachment_data(&raw, uid)
        })
        .await
        .map_err(|e| MailkitError::Other(format!("spawn_blocking join error: {}", e)))??;

        if attachments.is_empty() {
            return Ok(serde_json::json!({
                "mailbox": mailbox,
                "account": account,
                "uid": uid,
                "downloaded": [],
            }));
        }

        // Write files on a blocking thread (filesystem I/O)
        let output_dir = output_dir.to_path_buf();
        let downloaded = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
            std::fs::create_dir_all(&output_dir).map_err(|e| {
                MailkitError::Other(format!(
                    "Failed to create directory '{}': {}",
                    output_dir.display(),
                    e
                ))
            })?;

            let mut results = Vec::new();
            for (name, content_type, bytes) in &attachments {
                let filename = format!("{}_{}", uid, sanitize_filename(name));
                let path = output_dir.join(&filename);
                std::fs::write(&path, bytes).map_err(|e| {
                    MailkitError::Other(format!("Failed to write '{}': {}", path.display(), e))
                })?;

                results.push(serde_json::json!({
                    "filename": filename,
                    "path": path.display().to_string(),
                    "contentType": content_type,
                    "size": bytes.len(),
                }));
            }
            Ok(results)
        })
        .await
        .map_err(|e| MailkitError::Other(format!("spawn_blocking join error: {}", e)))??;

        Ok(serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "uid": uid,
            "downloaded": downloaded,
        }))
    }

    // -----------------------------------------------------------------
    // Unsubscribe
    // -----------------------------------------------------------------

    /// Fetch unsubscribe info, attempt RFC 8058 one-click unsubscribe,
    /// and optionally delete matching bulk messages across **all** mailboxes.
    ///
    /// Requires `List-Unsubscribe` header. Attempts one-click POST if
    /// `List-Unsubscribe-Post` is present. Deletion matches by exact sender +
    /// `List-Unsubscribe-Post` header to ensure only bulk/marketing mail is removed.
    pub async fn unsubscribe_message(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        delete_matching: bool,
        on_progress: Option<&ProgressFn>,
    ) -> Result<serde_json::Value> {
        let trash = self.trash_mailbox(account);

        let mut session = self.pool.acquire(account).await?;

        // Fetch unsubscribe + list-id headers from the target message
        let headers =
            imap_client::fetch_unsubscribe_headers(session.session(), mailbox, uid).await?;

        let has_unsubscribe = headers.list_unsubscribe.is_some();

        let mut result = serde_json::json!({
            "mailbox": mailbox,
            "account": account,
            "uid": uid,
            "listUnsubscribe": headers.list_unsubscribe,
            "listUnsubscribePost": headers.list_unsubscribe_post,
            "listId": headers.list_id,
        });

        if has_unsubscribe {
            let unsub_result = attempt_one_click_unsubscribe(
                headers.list_unsubscribe.as_deref(),
                headers.list_unsubscribe_post.as_deref(),
            )
            .await;
            result["unsubscribed"] = serde_json::json!(unsub_result);
            result["pathway"] = serde_json::json!("list-unsubscribe");

            if delete_matching {
                // Get the exact sender of the target message
                let (target_email, target_name) =
                    imap_client::fetch_sender(session.session(), uid).await?;

                let sender_display = if target_name.is_empty() {
                    target_email.clone()
                } else {
                    format!("{} <{}>", target_name, target_email)
                };

                // Search every mailbox for messages from this sender that
                // have a List-Unsubscribe-Post header (bulk/marketing mail)
                let all_mailboxes =
                    imap_client::list_mailbox_names(session.session()).await?;

                let mut total_found = 0usize;
                let mut total_deleted = 0usize;
                let mut total_failed = 0usize;
                let mut per_mailbox = Vec::new();

                for mbox in &all_mailboxes {
                    if imap_client::select(session.session(), mbox).await.is_err() {
                        continue;
                    }

                    let criteria = SearchCriteria {
                        from: Some(target_email.clone()),
                        deleted: Some(false),
                        ..Default::default()
                    };
                    let query = imap_client::build_search_query_pub(&criteria);
                    let candidate_uids =
                        imap_client::search_uids(session.session(), &query).await?;

                    if candidate_uids.is_empty() {
                        continue;
                    }

                    let exact_uids = filter_sender_with_unsub_post(
                        session.session(),
                        &candidate_uids,
                        &target_email,
                        &target_name,
                    )
                    .await?;

                    if exact_uids.is_empty() {
                        continue;
                    }

                    let found = exact_uids.len();
                    let (deleted, failed) = imap_client::bulk_delete_messages(
                        session.session(),
                        &exact_uids,
                        trash.as_deref(),
                        on_progress,
                    )
                    .await?;
                    imap_client::sync(session.session()).await?;

                    total_found += found;
                    total_deleted += deleted.len();
                    total_failed += failed.len();

                    if found > 0 {
                        per_mailbox.push(serde_json::json!({
                            "mailbox": mbox,
                            "found": found,
                            "deleted": deleted.len(),
                            "failed": failed.len(),
                        }));
                    }
                }

                result["matchingMessages"] = serde_json::json!({
                    "matchedBy": "sender+list-unsubscribe-post",
                    "sender": sender_display,
                    "found": total_found,
                    "deleted": total_deleted,
                    "failed": total_failed,
                    "mailboxes": per_mailbox,
                });
            }
        } else {
            result["unsubscribed"] = serde_json::json!({
                "success": false,
                "reason": "Message has no List-Unsubscribe header.",
            });
        }

        session.release().await;
        Ok(result)
    }

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

    fn trash_mailbox(&self, account: &str) -> Option<String> {
        self.pool
            .account_config(account)
            .and_then(|c| c.trash_mailbox.clone())
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// From a set of candidate UIDs, fetch FROM + List-Unsubscribe-Post headers and
/// return only those that match the exact sender AND have a List-Unsubscribe-Post header.
async fn filter_sender_with_unsub_post(
    session: &mut imap_client::ImapSession,
    candidate_uids: &[u32],
    target_email: &str,
    target_name: &str,
) -> Result<Vec<u32>> {
    let mut exact = Vec::new();
    for chunk in candidate_uids.chunks(500) {
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetched = imap_client::timed_uid_fetch_collect_pub(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (FROM List-Unsubscribe-Post)])",
        )
        .await?;

        for item in fetched {
            let fetch = item.map_err(MailkitError::Imap)?;
            let uid = match fetch.uid {
                Some(u) => u,
                None => continue,
            };
            let header_bytes = fetch.header().unwrap_or(&[]);
            let header_str = String::from_utf8_lossy(header_bytes);

            // Must have List-Unsubscribe-Post
            if imap_client::extract_header_value_pub(&header_str, "List-Unsubscribe-Post")
                .is_none()
            {
                continue;
            }

            // Must match exact sender
            if let Ok((email, name, _)) = parser::parse_sender_date(header_bytes) {
                if email == target_email && name == target_name {
                    exact.push(uid);
                }
            }
        }
    }
    Ok(exact)
}

/// Extract the first HTTPS URL from a `List-Unsubscribe` header value.
///
/// The header format is: `<https://example.com/unsub>, <mailto:unsub@example.com>`
fn extract_https_unsubscribe_url(header: &str) -> Option<String> {
    for part in header.split(',') {
        let trimmed = part.trim();
        if trimmed.starts_with('<') && trimmed.ends_with('>') {
            let url = &trimmed[1..trimmed.len() - 1];
            if url.starts_with("https://") || url.starts_with("http://") {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// Attempt RFC 8058 one-click unsubscribe.
///
/// Returns a JSON-friendly status object:
/// - `{ "success": true, "method": "one-click", "url": "..." }` on success
/// - `{ "success": false, "reason": "..." }` on failure or when not supported
async fn attempt_one_click_unsubscribe(
    list_unsubscribe: Option<&str>,
    list_unsubscribe_post: Option<&str>,
) -> serde_json::Value {
    // RFC 8058 requires both List-Unsubscribe (with https URL) and
    // List-Unsubscribe-Post: List-Unsubscribe=One-Click
    let post_value = match list_unsubscribe_post {
        Some(v) if v.contains("List-Unsubscribe=One-Click") => v,
        _ => {
            return serde_json::json!({
                "success": false,
                "reason": "No List-Unsubscribe-Post header with One-Click support. Manual unsubscribe may be required via the List-Unsubscribe URL.",
            });
        }
    };
    let _ = post_value;

    let unsub_header = match list_unsubscribe {
        Some(h) => h,
        None => {
            return serde_json::json!({
                "success": false,
                "reason": "No List-Unsubscribe header found.",
            });
        }
    };

    let url = match extract_https_unsubscribe_url(unsub_header) {
        Some(u) => u,
        None => {
            return serde_json::json!({
                "success": false,
                "reason": "No HTTPS URL found in List-Unsubscribe header. Only mailto: links are present.",
            });
        }
    };

    // Send the one-click POST request per RFC 8058
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "reason": format!("Failed to create HTTP client: {e}"),
            });
        }
    };

    match client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if resp.status().is_success() || resp.status().is_redirection() {
                serde_json::json!({
                    "success": true,
                    "method": "one-click",
                    "url": url,
                    "httpStatus": status,
                })
            } else {
                serde_json::json!({
                    "success": false,
                    "reason": format!("Unsubscribe endpoint returned HTTP {status}"),
                    "url": url,
                    "httpStatus": status,
                })
            }
        }
        Err(e) => {
            serde_json::json!({
                "success": false,
                "reason": format!("HTTP request failed: {e}"),
                "url": url,
            })
        }
    }
}

/// Clamp a value to a range with a default.
pub fn clamp_usize(val: Option<u64>, default: usize, min: usize, max: usize) -> usize {
    let v = val.map(|v| v as usize).unwrap_or(default);
    v.max(min).min(max)
}

/// Auto-detect the Drafts mailbox by scanning LIST results.
/// Checks common names: "Drafts", "[Gmail]/Drafts", "INBOX.Drafts".
async fn find_drafts_mailbox(session: &mut imap_client::ImapSession) -> Result<Option<String>> {
    let names = imap_client::list_mailbox_names(session).await?;
    let candidates = ["Drafts", "[Gmail]/Drafts", "INBOX.Drafts"];
    for candidate in &candidates {
        if let Some(name) = names.iter().find(|n| n.eq_ignore_ascii_case(candidate)) {
            return Ok(Some(name.clone()));
        }
    }
    // Fallback: look for any mailbox containing "draft" (case-insensitive)
    if let Some(name) = names.iter().find(|n| n.to_lowercase().contains("draft")) {
        return Ok(Some(name.clone()));
    }
    Ok(None)
}

/// Sanitize a filename: replace path separators and control chars with underscores.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Apple Mail flag color helpers (RFC draft-eggert-mailflagcolors-00)
// ---------------------------------------------------------------------------

/// Map a color name to [Bit0, Bit1, Bit2] booleans.
/// Returns None if the color name is unknown.
fn color_to_bits(color: &str) -> Option<[bool; 3]> {
    match color.to_lowercase().as_str() {
        "red" => Some([false, false, false]),
        "orange" => Some([true, false, false]),
        "yellow" => Some([false, true, false]),
        "green" => Some([false, true, true]),
        "blue" => Some([false, false, true]),
        "purple" => Some([true, false, true]),
        "gray" | "grey" => Some([true, true, false]),
        _ => None,
    }
}

/// Map [$MailFlagBit0, $MailFlagBit1, $MailFlagBit2] presence to a color name.
pub fn bits_to_color(flags: &[String]) -> Option<&'static str> {
    let bit0 = flags.iter().any(|f| f == "$MailFlagBit0");
    let bit1 = flags.iter().any(|f| f == "$MailFlagBit1");
    let bit2 = flags.iter().any(|f| f == "$MailFlagBit2");
    // Only meaningful when \Flagged is set
    if !flags.iter().any(|f| f == "\\Flagged") {
        return None;
    }
    match (bit0, bit1, bit2) {
        (false, false, false) => Some("red"),
        (true, false, false) => Some("orange"),
        (false, true, false) => Some("yellow"),
        (false, true, true) => Some("green"),
        (false, false, true) => Some("blue"),
        (true, false, true) => Some("purple"),
        (true, true, false) => Some("gray"),
        _ => None,
    }
}
