pub mod config;
pub mod connection;
pub mod content;
pub mod credentials;
pub mod draft;
pub mod error;
pub mod imap_client;
pub mod mcp;
pub mod parser;
pub mod provider;
pub mod scan_cache;
pub mod secret;
pub mod types;

pub use config::{AccountConfig, Config};
pub use connection::ConnectionPool;
pub use error::{AgentmailError, Result};
pub use imap_client::{CancelFn, ProgressFn};
pub use provider::MailProvider;
pub use secret::init_service_name;
pub use types::*;

/// Cached special-use mailbox names for one account (RFC 6154 Trash/Drafts),
/// with the resolution time so the entry can expire.
struct SpecialMailboxes {
    trash: Option<String>,
    drafts: Option<String>,
    resolved_at: std::time::Instant,
}

/// How long a resolved special-use mailbox stays cached. Matches the pool's
/// idle-session window so a moved/renamed folder is re-detected promptly.
const SPECIAL_MAILBOX_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// High-level facade for IMAP operations.
/// Owns the connection pool and configuration.
pub struct Agentmail {
    pool: ConnectionPool,
    /// Per-account Trash/Drafts resolution cache (one LIST resolves both).
    special_mailboxes: parking_lot::Mutex<hashbrown::HashMap<String, SpecialMailboxes>>,
    /// Per-(account, mailbox) header-scan cache backing the `rank_*` tools.
    scan_cache: parking_lot::Mutex<scan_cache::ScanCache>,
}

impl Agentmail {
    /// Create from an existing config.
    pub fn new(config: Config) -> Self {
        Self {
            pool: ConnectionPool::new(config),
            special_mailboxes: parking_lot::Mutex::new(hashbrown::HashMap::new()),
            scan_cache: parking_lot::Mutex::new(scan_cache::ScanCache::default()),
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

    /// Resolve server capabilities for an account, using the pool's cache and
    /// acquiring a session only on a cold miss.
    async fn caps_for(&self, account: &str) -> Result<std::sync::Arc<imap_client::ServerCaps>> {
        if let Some(caps) = self.pool.cached_caps(account) {
            return Ok(caps);
        }
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        session.release().await;
        Ok(caps)
    }

    /// Get the underlying config.
    pub fn config(&self) -> &Config {
        self.pool.config()
    }

    // -----------------------------------------------------------------
    // Account & connection
    // -----------------------------------------------------------------

    /// List configured accounts.
    pub async fn list_accounts(&self) -> Result<ListAccountsResponse> {
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

        Ok(ListAccountsResponse { accounts })
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
    pub async fn list_capabilities(&self, account: &str) -> Result<ListCapabilitiesResponse> {
        let caps = self
            .pool
            .with_session_retry(account, async |s| imap_client::list_capabilities(s).await)
            .await?;

        Ok(ListCapabilitiesResponse {
            account: account.to_string(),
            capabilities: caps,
        })
    }

    // -----------------------------------------------------------------
    // Mailboxes
    // -----------------------------------------------------------------

    /// List mailboxes, optionally scoped to an account.
    pub async fn list_mailboxes(&self, account: Option<&str>) -> Result<ListMailboxesResponse> {
        let account_names: Vec<String> = if let Some(name) = account {
            if !self.pool.config().accounts.contains_key(name) {
                return Err(AgentmailError::AccountNotFound(name.to_string()));
            }
            vec![name.to_string()]
        } else {
            self.pool.account_names()
        };

        let mut mailboxes: Vec<MailboxInfo> = Vec::new();
        for acct_name in &account_names {
            let acct = acct_name.clone();
            // Resolve caps once (cached after the first fetch) so the retry
            // closure captures only the owned Arc — capturing `&pool` would
            // make the async closure's lifetime too short for `Send`.
            let caps = self.caps_for(acct_name).await?;
            let mboxes = self
                .pool
                .with_session_retry(acct_name, async move |s| {
                    imap_client::list_mailboxes(s, &acct, &caps).await
                })
                .await?;
            mailboxes.extend(mboxes);
        }

        Ok(ListMailboxesResponse { mailboxes })
    }

    /// Create a new mailbox on the server.
    pub async fn create_mailbox(
        &self,
        account: &str,
        mailbox_name: &str,
    ) -> Result<CreateMailboxResponse> {
        let mut session = self.pool.acquire(account).await?;

        // Check if mailbox already exists (make CREATE idempotent)
        let names = imap_client::list_mailbox_names(session.session()).await?;
        if names.iter().any(|n| n.eq_ignore_ascii_case(mailbox_name)) {
            session.release().await;
            return Ok(CreateMailboxResponse {
                account: account.to_string(),
                mailbox: mailbox_name.to_string(),
                created: false,
                already_exists: true,
            });
        }

        imap_client::create_mailbox(session.session(), mailbox_name).await?;
        imap_client::sync(session.session()).await?;
        session.release().await;

        // A new mailbox may be the Drafts/Trash folder — drop the cached
        // special-use resolution so it is re-detected on next use.
        self.invalidate_special_mailboxes(account);

        Ok(CreateMailboxResponse {
            account: account.to_string(),
            mailbox: mailbox_name.to_string(),
            created: true,
            already_exists: false,
        })
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
    ) -> Result<GetMessagesResponse> {
        let (mailbox_s, account_s) = (mailbox.to_string(), account.to_string());
        let (messages, total) = self
            .pool
            .with_session_retry(account, async move |s| {
                imap_client::fetch_messages(
                    s,
                    &mailbox_s,
                    &account_s,
                    offset,
                    limit,
                    include_content,
                    include_headers,
                )
                .await
            })
            .await?;

        Ok(GetMessagesResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            offset,
            limit,
            total: total as usize,
            messages,
        })
    }

    /// Fetch specific messages by UID.
    pub async fn get_messages_by_uid(
        &self,
        mailbox: &str,
        account: &str,
        uids: &[u32],
        include_content: bool,
        include_headers: bool,
    ) -> Result<GetMessagesByUidResponse> {
        let (mailbox_s, account_s, uids_v) =
            (mailbox.to_string(), account.to_string(), uids.to_vec());
        let messages = self
            .pool
            .with_session_retry(account, async move |s| {
                imap_client::select(s, &mailbox_s).await?;
                imap_client::fetch_by_uids(
                    s,
                    &uids_v,
                    &mailbox_s,
                    &account_s,
                    include_content,
                    include_headers,
                )
                .await
            })
            .await?;

        Ok(GetMessagesByUidResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            messages,
        })
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
    ) -> Result<SearchMessagesResponse> {
        let (mailbox_s, account_s, criteria_c) =
            (mailbox.to_string(), account.to_string(), criteria.clone());
        let (messages, total) = self
            .pool
            .with_session_retry(account, async move |s| {
                imap_client::search_messages(
                    s,
                    &mailbox_s,
                    &account_s,
                    &criteria_c,
                    offset,
                    limit,
                    include_content,
                    include_headers,
                )
                .await
            })
            .await?;

        Ok(SearchMessagesResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            offset,
            limit,
            total_matches: total as usize,
            messages,
        })
    }

    // -----------------------------------------------------------------
    // Group by sender
    // -----------------------------------------------------------------

    /// Group messages by sender address with counts and date ranges.
    /// Sorted by message count descending.
    ///
    /// When `mailbox` is `None`, scans all mailboxes in the account.
    pub async fn rank_senders(
        &self,
        mailbox: Option<&str>,
        account: &str,
        limit: Option<usize>,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<RankSendersResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => list_scannable_mailbox_names(session.session()).await?,
        };

        use hashbrown::{HashMap, HashSet};
        // Key by (email, display_name) so "Find My <noreply@apple.com>" and
        // "iCloud <noreply@apple.com>" are separate entries.
        let mut map: HashMap<(String, String), SenderSummary> = HashMap::new();
        // Dedup the same logical message across folders (Gmail labels / All Mail).
        let mut seen: HashSet<String> = HashSet::new();

        for mbox in &mailboxes {
            imap_client::check_cancel(cancel)?;
            let sender_dates = match self
                .cached_sender_rows(account, mbox, session.session(), on_progress, cancel)
                .await
            {
                Ok(data) => data,
                Err(_) => continue, // skip unselectable mailboxes
            };

            for row in sender_dates {
                if row.email.is_empty() {
                    continue;
                }
                if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                    continue; // already counted this message from another folder
                }
                let key = (row.email.clone(), row.display_name.clone());
                let entry = map.entry(key).or_insert_with(|| SenderSummary {
                    sender: String::new(),
                    address: row.email.clone(),
                    display_name: row.display_name.clone(),
                    count: 0,
                    oldest_date: None,
                    newest_date: None,
                });

                entry.count += 1;

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

        imap_client::check_cancel(cancel)?;
        session.release().await;

        let mut senders: Vec<SenderSummary> = map.into_values().collect();
        for s in &mut senders {
            s.sender = if s.display_name.is_empty() {
                s.address.clone()
            } else {
                format!("{} <{}>", s.display_name, s.address)
            };
        }
        senders.sort_by_key(|b| std::cmp::Reverse(b.count));

        let unique_senders = senders.len();
        let total_messages = senders.iter().map(|s| s.count).sum::<u32>();
        if let Some(n) = limit {
            senders.truncate(n);
        }

        Ok(RankSendersResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_senders,
            senders,
        })
    }

    /// Group mailing-list messages by sender.
    ///
    /// Includes messages that have List-Unsubscribe and/or List-Id.
    /// Groups by exact sender (email + display name). The sample_uid and
    /// unsubscribe info come from the newest message in each group.
    ///
    /// When `mailbox` is `None`, scans all mailboxes in the account.
    pub async fn rank_unsubscribe(
        &self,
        mailbox: Option<&str>,
        account: &str,
        limit: Option<usize>,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<RankUnsubscribeResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => list_scannable_mailbox_names(session.session()).await?,
        };

        use hashbrown::{HashMap, HashSet};
        use types::ListSummary;

        // Key by (email, display_name) for exact sender grouping
        let mut map: HashMap<(String, String), ListSummary> = HashMap::new();
        // Dedup the same logical message across folders (Gmail labels / All Mail).
        let mut seen: HashSet<String> = HashSet::new();

        for mbox in &mailboxes {
            imap_client::check_cancel(cancel)?;
            let rows = match self
                .cached_list_rows(account, mbox, session.session(), on_progress, cancel)
                .await
            {
                Ok(data) => data,
                Err(_) => continue, // skip unselectable mailboxes
            };

            for row in rows {
                if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                    continue; // already counted this message from another folder
                }
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
                        sample_uid: row.uid,
                        sample_mailbox: Some(mbox.clone()),
                        count: 0,
                        oldest_date: None,
                        newest_date: None,
                    }
                });

                entry.count += 1;

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
                    if !row.sender_name.is_empty() {
                        entry.display_name = row.sender_name.clone();
                        entry.sender = format!("{} <{}>", entry.display_name, row.sender_email);
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

        imap_client::check_cancel(cancel)?;
        session.release().await;

        let mut lists: Vec<ListSummary> = map.into_values().collect();
        // One-click senders first, then by message count
        lists.sort_by(|a, b| {
            b.one_click
                .cmp(&a.one_click)
                .then_with(|| b.count.cmp(&a.count))
        });

        let unique_lists = lists.len();
        let total_messages = lists.iter().map(|l| l.count).sum::<u32>();
        if let Some(n) = limit {
            lists.truncate(n);
        }

        Ok(RankUnsubscribeResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_lists,
            lists,
        })
    }

    /// Group messages by List-Id header (RFC 2919).
    ///
    /// Groups all messages from the same mailing list regardless of sender.
    /// When `mailbox` is `None`, scans all mailboxes (excluding trash/junk/drafts).
    pub async fn rank_list_id(
        &self,
        mailbox: Option<&str>,
        account: &str,
        limit: Option<usize>,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<RankListIdResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => list_scannable_mailbox_names(session.session()).await?,
        };

        use hashbrown::{HashMap, HashSet};

        struct ListIdEntry {
            display_name: String,
            senders: HashSet<String>,
            count: u32,
            sample_uid: u32,
            sample_mailbox: Option<String>,
            oldest_date: Option<chrono::DateTime<chrono::Utc>>,
            newest_date: Option<chrono::DateTime<chrono::Utc>>,
        }

        let mut map: HashMap<String, ListIdEntry> = HashMap::new();
        // Dedup the same logical message across folders (Gmail labels / All Mail).
        let mut seen: HashSet<String> = HashSet::new();

        for mbox in &mailboxes {
            imap_client::check_cancel(cancel)?;
            let rows = match self
                .cached_list_rows(account, mbox, session.session(), on_progress, cancel)
                .await
            {
                Ok(data) => data,
                Err(_) => continue,
            };

            for row in rows {
                let list_id = match row.list_id {
                    Some(ref id) if !id.is_empty() => id.clone(),
                    _ => continue, // Skip messages without List-Id
                };
                if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                    continue; // already counted this message from another folder
                }

                let entry = map.entry(list_id.clone()).or_insert_with(|| {
                    let display = extract_list_id_display(&list_id);
                    ListIdEntry {
                        display_name: display,
                        senders: HashSet::new(),
                        count: 0,
                        sample_uid: row.uid,
                        sample_mailbox: Some(mbox.clone()),
                        oldest_date: None,
                        newest_date: None,
                    }
                });

                entry.count += 1;
                if !row.sender_email.is_empty() {
                    entry.senders.insert(row.sender_email.clone());
                }

                let is_newer = match row.date {
                    Some(d) => entry.newest_date.map(|e| d > e).unwrap_or(true),
                    None => entry.newest_date.is_none(),
                };
                if is_newer {
                    entry.sample_uid = row.uid;
                    entry.sample_mailbox = Some(mbox.clone());
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

        imap_client::check_cancel(cancel)?;
        session.release().await;

        let mut lists: Vec<ListIdSummary> = map
            .into_iter()
            .map(|(list_id, entry)| {
                let mut senders: Vec<String> = entry.senders.into_iter().collect();
                senders.sort();
                ListIdSummary {
                    list_id,
                    display_name: entry.display_name,
                    senders,
                    count: entry.count,
                    sample_uid: entry.sample_uid,
                    sample_mailbox: entry.sample_mailbox,
                    oldest_date: entry.oldest_date,
                    newest_date: entry.newest_date,
                }
            })
            .collect();
        lists.sort_by_key(|b| std::cmp::Reverse(b.count));

        let unique_lists = lists.len();
        let total_messages = lists.iter().map(|l| l.count).sum::<u32>();
        if let Some(n) = limit {
            lists.truncate(n);
        }

        Ok(RankListIdResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_lists,
            lists,
        })
    }

    /// Delete all messages with a specific List-Id across mailboxes.
    pub async fn delete_list_id(
        &self,
        mailbox: Option<&str>,
        account: &str,
        list_id: &str,
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<DeleteListIdResponse> {
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self.trash_for_mode(mode, account, session.session()).await;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => list_scannable_mailbox_names(session.session()).await?,
        };

        let mut total_found = 0usize;
        let mut total_deleted = 0usize;
        let mut total_failed = 0usize;
        let mut per_mailbox = Vec::new();
        let mut skipped = Vec::new();

        for mbox in &mailboxes {
            imap_client::check_cancel(cancel)?;
            if imap_client::select(session.session(), mbox).await.is_err() {
                skipped.push(mbox.clone());
                continue;
            }

            // Server-side SEARCH HEADER List-Id
            let criteria = SearchCriteria {
                header: Some(("List-Id".to_string(), list_id.to_string())),
                deleted: Some(false),
                ..Default::default()
            };
            let query = imap_client::build_search_query_pub(&criteria)?;
            let candidates = match imap_client::search_uids(session.session(), &query).await {
                Ok(u) => u,
                Err(_) => {
                    skipped.push(mbox.clone());
                    continue;
                }
            };

            if candidates.is_empty() {
                continue;
            }

            // IMAP HEADER search is substring-only, so "news" would also match
            // "newsletter". Confirm the exact List-Id before deleting.
            let with_ids =
                imap_client::fetch_list_ids_for_uids(session.session(), &candidates).await?;
            let uids: Vec<u32> = with_ids
                .into_iter()
                .filter(|(_, id)| id.as_deref().is_some_and(|v| list_id_matches(list_id, v)))
                .map(|(uid, _)| uid)
                .collect();

            if uids.is_empty() {
                continue;
            }

            let found = uids.len();
            let result = imap_client::bulk_delete_messages(
                session.session(),
                &uids,
                trash.as_deref(),
                &caps,
                on_progress,
                cancel,
            )
            .await?;
            imap_client::sync(session.session()).await?;

            total_found += found;
            total_deleted += result.deleted.len();
            total_failed += result.failed.len();

            if found > 0 {
                per_mailbox.push(PerMailboxDeleteResult {
                    mailbox: mbox.clone(),
                    found,
                    deleted: result.deleted.len(),
                    failed: result.failed.len(),
                });
            }
        }

        session.release().await;
        self.invalidate_scan_cache(account);

        Ok(DeleteListIdResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            list_id: list_id.to_string(),
            found: total_found,
            deleted: total_deleted,
            failed: total_failed,
            mailboxes: per_mailbox,
            skipped,
            permanent: mode == DeleteMode::Permanent,
        })
    }

    // -----------------------------------------------------------------
    // Flags
    // -----------------------------------------------------------------

    /// List all flags actually in use across messages, with counts.
    ///
    /// When `mailbox` is `None`, scans all mailboxes in the account.
    /// Resolves Apple Mail `$MailFlagBit*` combinations to color names per-message.
    pub async fn list_flags(
        &self,
        mailbox: Option<&str>,
        account: &str,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<ListFlagsResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => list_scannable_mailbox_names(session.session()).await?,
        };

        use hashbrown::HashMap;
        let mut total_flags: HashMap<String, u32> = HashMap::new();
        let mut total_colors: HashMap<String, u32> = HashMap::new();
        let mut per_mailbox = Vec::new();

        for mbox in &mailboxes {
            imap_client::check_cancel(cancel)?;
            let scan = match imap_client::fetch_flags(session.session(), mbox, on_progress, cancel)
                .await
            {
                Ok(s) => s,
                Err(_) => continue, // skip unselectable mailboxes
            };

            if !scan.flags.is_empty() {
                let mbox_flags: Vec<FlagCount> = scan
                    .flags
                    .iter()
                    .map(|(name, count)| FlagCount {
                        flag: name.clone(),
                        count: *count,
                    })
                    .collect();
                per_mailbox.push(MailboxFlagBreakdown {
                    mailbox: mbox.clone(),
                    total_flags: mbox_flags.len(),
                    flags: mbox_flags,
                });
            }

            // entry_ref: per-mailbox aggregation over a tiny distinct flag/color
            // set — almost always a hit. Allocate the owned key only on insert.
            // See PERF-entry-ref.md.
            for (name, count) in &scan.flags {
                *total_flags.entry_ref(name.as_str()).or_insert(0) += count;
            }
            for (color, count) in &scan.colors {
                *total_colors.entry_ref(color.as_str()).or_insert(0) += count;
            }
        }

        imap_client::check_cancel(cancel)?;
        session.release().await;

        let mut flag_list: Vec<(String, u32)> = total_flags.into_iter().collect();
        flag_list.sort_by_key(|b| std::cmp::Reverse(b.1));
        let flags: Vec<FlagCount> = flag_list
            .into_iter()
            .map(|(flag, count)| FlagCount { flag, count })
            .collect();

        let mut color_list: Vec<(String, u32)> = total_colors.into_iter().collect();
        color_list.sort_by_key(|b| std::cmp::Reverse(b.1));
        let colors: Vec<ColorCount> = color_list
            .into_iter()
            .map(|(color, count)| ColorCount { color, count })
            .collect();

        Ok(ListFlagsResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_flags: flags.len(),
            flags,
            colors,
            per_mailbox,
        })
    }

    /// Add flags and/or set a color on a message.
    /// Flags use union semantics (+FLAGS). Color replaces any existing color.
    pub async fn add_flags(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        flags: &[String],
        color: Option<&str>,
    ) -> Result<UpdateFlagsResponse> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;

        // Set color if requested (clear old bits, set new ones)
        if let Some(color_name) = color {
            let bits = color_to_bits(color_name).ok_or_else(|| {
                AgentmailError::Other(format!(
                    "Unknown flag color '{}'. Valid: red, orange, yellow, green, blue, purple, gray",
                    color_name
                ))
            })?;
            let color_bits = ["$MailFlagBit0", "$MailFlagBit1", "$MailFlagBit2"];
            imap_client::remove_flags(
                session.session(),
                uid,
                &color_bits.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .await?;
            let mut add = vec!["\\Flagged".to_string()];
            for (i, &bit) in color_bits.iter().enumerate() {
                if bits[i] {
                    add.push(bit.to_string());
                }
            }
            imap_client::add_flags(session.session(), uid, &add).await?;
        }

        // Add regular flags
        if !flags.is_empty() {
            imap_client::add_flags(session.session(), uid, flags).await?;
        }

        imap_client::sync(session.session()).await?;
        let updated_flags = imap_client::get_flags(session.session(), uid).await?;
        let resolved_color = bits_to_color(&updated_flags).map(|c| c.to_string());
        session.release().await;

        Ok(UpdateFlagsResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            flags: updated_flags,
            color: resolved_color,
        })
    }

    /// Remove flags and/or clear color from a message.
    /// Flags use difference semantics (-FLAGS). `remove_color` clears \Flagged + all color bits.
    pub async fn remove_flags(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        flags: &[String],
        remove_color: bool,
    ) -> Result<UpdateFlagsResponse> {
        let mut session = self.pool.acquire(account).await?;
        imap_client::select(session.session(), mailbox).await?;

        // Remove color if requested
        if remove_color {
            let mut remove = vec!["\\Flagged".to_string()];
            remove.extend(
                ["$MailFlagBit0", "$MailFlagBit1", "$MailFlagBit2"]
                    .iter()
                    .map(|s| s.to_string()),
            );
            imap_client::remove_flags(session.session(), uid, &remove).await?;
        }

        // Remove regular flags
        if !flags.is_empty() {
            imap_client::remove_flags(session.session(), uid, flags).await?;
        }

        imap_client::sync(session.session()).await?;
        let updated_flags = imap_client::get_flags(session.session(), uid).await?;
        let resolved_color = bits_to_color(&updated_flags).map(|c| c.to_string());
        session.release().await;

        Ok(UpdateFlagsResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            flags: updated_flags,
            color: resolved_color,
        })
    }

    // -----------------------------------------------------------------
    // Attachments
    // -----------------------------------------------------------------

    /// Find messages with attachments via Content-Type header scan.
    /// Returns UIDs and total count; use get_messages_by_uid to fetch details.
    ///
    /// When `mailbox` is `None`, scans all mailboxes in the account and
    /// includes a per-mailbox breakdown in the output.
    pub async fn find_attachments(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<FindAttachmentsResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => list_scannable_mailbox_names(session.session()).await?,
        };

        let mut all_uids: Vec<u32> = Vec::new();
        let mut per_mailbox = Vec::new();

        for mbox in &mailboxes {
            imap_client::check_cancel(cancel)?;
            let uids = match imap_client::fetch_attachment_uids(
                session.session(),
                mbox,
                on_progress,
                cancel,
            )
            .await
            {
                Ok(u) => u,
                Err(_) => continue, // skip unselectable mailboxes
            };

            if !uids.is_empty() {
                per_mailbox.push(MailboxAttachmentCount {
                    mailbox: mbox.clone(),
                    count: uids.len(),
                });
                all_uids.extend(uids);
            }
        }

        imap_client::check_cancel(cancel)?;
        session.release().await;

        // Sort newest-first (highest UID first) across all mailboxes
        all_uids.sort_unstable_by(|a, b| b.cmp(a));

        let total = all_uids.len();
        let page: Vec<u32> = all_uids.into_iter().skip(offset).take(limit).collect();

        Ok(FindAttachmentsResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total,
            offset,
            limit,
            uids: page,
            per_mailbox,
        })
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
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<DeleteMessagesResponse> {
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self.trash_for_mode(mode, account, session.session()).await;
        imap_client::select(session.session(), mailbox).await?;
        let result = imap_client::bulk_delete_messages(
            session.session(),
            uids,
            trash.as_deref(),
            &caps,
            on_progress,
            cancel,
        )
        .await?;
        imap_client::sync(session.session()).await?;
        session.release().await;
        self.invalidate_scan_cache(account);

        Ok(DeleteMessagesResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            deleted: result.deleted.len(),
            failed: result.failed.len(),
            trash_fallback: result.trash_fallback,
            permanent: mode == DeleteMode::Permanent,
        })
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
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<DeleteBySenderResponse> {
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self.trash_for_mode(mode, account, session.session()).await;
        imap_client::select(session.session(), mailbox).await?;

        // 1. Fetch the exact sender from the target message
        let (target_email, target_name) = imap_client::fetch_sender(session.session(), uid).await?;

        let sender_display = if target_name.is_empty() {
            target_email.clone()
        } else {
            format!("{} <{}>", target_name, target_email)
        };

        let search_mailboxes = if all_mailboxes {
            list_scannable_mailbox_names(session.session()).await?
        } else {
            vec![mailbox.to_string()]
        };

        let mut total_found = 0usize;
        let mut total_deleted = 0usize;
        let mut total_failed = 0usize;
        let mut per_mailbox = Vec::new();
        let mut skipped = Vec::new();

        for mbox in &search_mailboxes {
            imap_client::check_cancel(cancel)?;
            if imap_client::select(session.session(), mbox).await.is_err() {
                skipped.push(mbox.clone());
                continue;
            }

            // Server-side FROM search (substring) to get candidates
            let criteria = SearchCriteria {
                from: Some(target_email.clone()),
                deleted: Some(false),
                ..Default::default()
            };
            let query = imap_client::build_search_query_pub(&criteria)?;
            let candidate_uids = match imap_client::search_uids(session.session(), &query).await {
                Ok(uids) => uids,
                Err(_) => {
                    skipped.push(mbox.clone());
                    continue;
                }
            };

            if candidate_uids.is_empty() {
                continue;
            }

            // Fetch FROM for all candidates and filter for exact match
            let candidates =
                imap_client::fetch_senders_batch(session.session(), &candidate_uids, cancel)
                    .await?;
            let exact_uids: Vec<u32> = candidates
                .into_iter()
                .filter(|(_uid, email, name)| email == &target_email && name == &target_name)
                .map(|(uid, _, _)| uid)
                .collect();

            if exact_uids.is_empty() {
                continue;
            }

            let found = exact_uids.len();
            let result = imap_client::bulk_delete_messages(
                session.session(),
                &exact_uids,
                trash.as_deref(),
                &caps,
                on_progress,
                cancel,
            )
            .await?;
            imap_client::sync(session.session()).await?;

            total_found += found;
            total_deleted += result.deleted.len();
            total_failed += result.failed.len();

            if found > 0 {
                per_mailbox.push(PerMailboxDeleteResult {
                    mailbox: mbox.clone(),
                    found,
                    deleted: result.deleted.len(),
                    failed: result.failed.len(),
                });
            }
        }

        session.release().await;
        self.invalidate_scan_cache(account);

        Ok(DeleteBySenderResponse {
            mailbox: if all_mailboxes {
                "*".to_string()
            } else {
                mailbox.to_string()
            },
            account: account.to_string(),
            sender: sender_display,
            found: total_found,
            deleted: total_deleted,
            failed: total_failed,
            mailboxes: per_mailbox,
            skipped,
            permanent: mode == DeleteMode::Permanent,
        })
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
    ) -> Result<MoveMessageResponse> {
        let mut session = self.pool.acquire(account).await?;

        // Validate destination mailbox exists
        let names = imap_client::list_mailbox_names(session.session()).await?;
        if !names.iter().any(|n| n.eq_ignore_ascii_case(destination)) {
            session.release().await;
            return Err(AgentmailError::Other(format!(
                "Destination mailbox '{}' does not exist",
                destination
            )));
        }

        let caps = self.pool.server_caps(account, session.session()).await?;
        imap_client::select(session.session(), mailbox).await?;
        imap_client::move_message(session.session(), uid, destination, &caps).await?;
        imap_client::sync(session.session()).await?;
        session.release().await;
        self.invalidate_scan_cache(account);

        Ok(MoveMessageResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            destination: destination.to_string(),
            moved: true,
        })
    }

    // -----------------------------------------------------------------
    // Draft
    // -----------------------------------------------------------------

    /// Create a draft message.
    /// When `attachments` is non-empty, the message is built as multipart/mixed
    /// with the body as the first part and each attachment as subsequent parts.
    pub async fn create_draft(
        &self,
        account: &str,
        subject: &str,
        body: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
        attachments: &[crate::types::DraftAttachment],
    ) -> Result<CreateDraftResponse> {
        if to.is_empty() && cc.is_empty() && bcc.is_empty() {
            return Err(AgentmailError::Other(
                "At least one recipient (to, cc, or bcc) is required".to_string(),
            ));
        }

        let acct_config = self
            .pool
            .account_config(account)
            .ok_or_else(|| AgentmailError::AccountNotFound(account.to_string()))?;
        let from = &acct_config.username;

        let rfc822 = draft::compose_draft(subject, body, to, cc, bcc, Some(from), attachments)?;

        let mut session = self.pool.acquire(account).await?;

        let (_, drafts) = self.special_mailboxes(account, session.session()).await?;
        let drafts_name = drafts.unwrap_or_else(|| "Drafts".to_string());

        // Best-effort: create the drafts mailbox if it doesn't exist yet.
        // Many servers auto-create it, but some (Dovecot, etc.) require explicit CREATE first.
        // Ignore "already exists" errors; the APPEND below will surface real problems.
        let _ = imap_client::create_mailbox(session.session(), &drafts_name).await;

        imap_client::append_draft(session.session(), &drafts_name, &rfc822).await?;
        imap_client::sync(session.session()).await?;
        session.release().await;

        let attached_names: Vec<String> = attachments.iter().map(|a| a.filename.clone()).collect();

        Ok(CreateDraftResponse {
            created: true,
            account: account.to_string(),
            drafts_mailbox: drafts_name,
            subject: subject.to_string(),
            recipients: DraftRecipients {
                to: to.to_vec(),
                cc: cc.to_vec(),
                bcc: bcc.to_vec(),
            },
            attachments: attached_names,
        })
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
    ) -> Result<GetMessageSourceResponse> {
        let mut session = self.pool.acquire(account).await?;
        let raw = imap_client::get_message_source(session.session(), mailbox, uid).await?;
        session.release().await;

        let source = String::from_utf8_lossy(&raw).to_string();
        Ok(GetMessageSourceResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            source,
        })
    }

    // -----------------------------------------------------------------
    // Download attachments
    // -----------------------------------------------------------------

    /// Download attachments from a message to a directory.
    /// Files are named `{uid}_{index}_{original_name}`.
    pub async fn download_attachments(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        output_dir: &std::path::Path,
    ) -> Result<DownloadAttachmentsResponse> {
        let mut session = self.pool.acquire(account).await?;
        let raw = imap_client::get_message_source(session.session(), mailbox, uid).await?;
        session.release().await;

        // Parse attachments on a blocking thread (CPU-intensive MIME parsing)
        let attachments =
            tokio::task::spawn_blocking(move || parser::extract_attachment_data(&raw, uid))
                .await
                .map_err(|e| {
                    AgentmailError::Other(format!("spawn_blocking join error: {}", e))
                })??;

        if attachments.is_empty() {
            return Ok(DownloadAttachmentsResponse {
                mailbox: mailbox.to_string(),
                account: account.to_string(),
                uid,
                downloaded: Vec::new(),
            });
        }

        // Write files using async I/O
        let output_dir = output_dir.to_path_buf();
        tokio::fs::create_dir_all(&output_dir).await.map_err(|e| {
            AgentmailError::Other(format!(
                "Failed to create directory '{}': {}",
                output_dir.display(),
                e
            ))
        })?;

        let mut downloaded = Vec::new();
        for (index, (name, content_type, bytes)) in attachments.iter().enumerate() {
            let filename = format!("{}_{}_{}", uid, index, sanitize_filename(name));
            let path = output_dir.join(&filename);
            tokio::fs::write(&path, bytes).await.map_err(|e| {
                AgentmailError::Other(format!("Failed to write '{}': {}", path.display(), e))
            })?;

            downloaded.push(DownloadedFile {
                index,
                filename,
                path: path.display().to_string(),
                content_type: content_type.clone(),
                size: bytes.len(),
            });
        }

        Ok(DownloadAttachmentsResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            downloaded,
        })
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
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<UnsubscribeResponse> {
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self.trash_for_mode(mode, account, session.session()).await;

        // Fetch unsubscribe + list-id headers from the target message
        let headers =
            imap_client::fetch_unsubscribe_headers(session.session(), mailbox, uid).await?;

        let has_unsubscribe = headers.list_unsubscribe.is_some();

        let mut response = UnsubscribeResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            list_unsubscribe: headers.list_unsubscribe.clone(),
            list_unsubscribe_post: headers.list_unsubscribe_post.clone(),
            list_id: headers.list_id.clone(),
            pathway: None,
            unsubscribed: UnsubscribeResult {
                success: false,
                method: None,
                url: None,
                http_status: None,
                reason: None,
            },
            matching_messages: None,
        };

        if has_unsubscribe {
            response.unsubscribed = attempt_one_click_unsubscribe(
                headers.list_unsubscribe.as_deref(),
                headers.list_unsubscribe_post.as_deref(),
            )
            .await;
            response.pathway = Some("list-unsubscribe".to_string());

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
                // have a List-Unsubscribe header (bulk/marketing mail)
                let all_mailboxes = list_scannable_mailbox_names(session.session()).await?;

                let mut total_found = 0usize;
                let mut total_deleted = 0usize;
                let mut total_failed = 0usize;
                let mut per_mailbox = Vec::new();
                let mut skipped = Vec::new();

                for mbox in &all_mailboxes {
                    imap_client::check_cancel(cancel)?;
                    if imap_client::select(session.session(), mbox).await.is_err() {
                        skipped.push(mbox.clone());
                        continue;
                    }

                    let criteria = SearchCriteria {
                        from: Some(target_email.clone()),
                        deleted: Some(false),
                        ..Default::default()
                    };
                    let query = imap_client::build_search_query_pub(&criteria)?;
                    let candidate_uids =
                        match imap_client::search_uids(session.session(), &query).await {
                            Ok(uids) => uids,
                            Err(_) => {
                                skipped.push(mbox.clone());
                                continue;
                            }
                        };

                    if candidate_uids.is_empty() {
                        continue;
                    }

                    let exact_uids = filter_sender_bulk_mail(
                        session.session(),
                        &candidate_uids,
                        &target_email,
                        &target_name,
                        cancel,
                    )
                    .await?;

                    if exact_uids.is_empty() {
                        continue;
                    }

                    let found = exact_uids.len();
                    let result = imap_client::bulk_delete_messages(
                        session.session(),
                        &exact_uids,
                        trash.as_deref(),
                        &caps,
                        on_progress,
                        cancel,
                    )
                    .await?;
                    imap_client::sync(session.session()).await?;

                    total_found += found;
                    total_deleted += result.deleted.len();
                    total_failed += result.failed.len();

                    if found > 0 {
                        per_mailbox.push(PerMailboxDeleteResult {
                            mailbox: mbox.clone(),
                            found,
                            deleted: result.deleted.len(),
                            failed: result.failed.len(),
                        });
                    }
                }

                response.matching_messages = Some(MatchingMessagesResult {
                    matched_by: "sender+list-unsubscribe".to_string(),
                    sender: sender_display,
                    found: total_found,
                    deleted: total_deleted,
                    failed: total_failed,
                    mailboxes: per_mailbox,
                    skipped,
                    permanent: mode == DeleteMode::Permanent,
                });
            }
        } else {
            response.unsubscribed = UnsubscribeResult {
                success: false,
                method: None,
                url: None,
                http_status: None,
                reason: Some("Message has no List-Unsubscribe header.".to_string()),
            };
        }

        session.release().await;
        // Deletion may have removed messages — drop stale rank-scan rows.
        if response.matching_messages.is_some() {
            self.invalidate_scan_cache(account);
        }
        Ok(response)
    }

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

    /// Resolve this account's Trash and Drafts mailbox names, caching the
    /// result for `SPECIAL_MAILBOX_TTL` so repeated delete/draft calls don't
    /// re-LIST every time. One LIST resolves both roles.
    async fn special_mailboxes(
        &self,
        account: &str,
        session: &mut imap_client::ImapSession,
    ) -> Result<(Option<String>, Option<String>)> {
        // Fast path: a fresh cached entry. Scoped guard, no await held.
        {
            let cache = self.special_mailboxes.lock();
            if let Some(entry) = cache.get(account)
                && entry.resolved_at.elapsed() < SPECIAL_MAILBOX_TTL
            {
                return Ok((entry.trash.clone(), entry.drafts.clone()));
            }
        }

        let entries = imap_client::list_mailbox_entries(session).await?;
        let trash = resolve_trash_from(&entries);
        let drafts = resolve_drafts_from(&entries);

        self.special_mailboxes.lock().insert(
            account.to_string(),
            SpecialMailboxes {
                trash: trash.clone(),
                drafts: drafts.clone(),
                resolved_at: std::time::Instant::now(),
            },
        );
        Ok((trash, drafts))
    }

    /// Resolve the trash destination for a delete, honoring the delete mode:
    /// `Permanent` bypasses Trash entirely (so deletion goes straight to
    /// flag + UID EXPUNGE).
    async fn trash_for_mode(
        &self,
        mode: DeleteMode,
        account: &str,
        session: &mut imap_client::ImapSession,
    ) -> Option<String> {
        match mode {
            DeleteMode::Permanent => None,
            DeleteMode::TrashFirst => self
                .special_mailboxes(account, session)
                .await
                .ok()
                .and_then(|(trash, _)| trash),
        }
    }

    /// Invalidate the cached special-use mailboxes for an account (e.g. after
    /// creating a mailbox that may now be the Drafts/Trash folder).
    fn invalidate_special_mailboxes(&self, account: &str) {
        self.special_mailboxes.lock().remove(account);
    }

    /// Drop all cached rank-scan rows for an account after a mutation that
    /// changes the message set (delete/move). The STATUS revalidation makes the
    /// cache correct without this, but it frees known-stale rows immediately.
    fn invalidate_scan_cache(&self, account: &str) {
        self.scan_cache.lock().invalidate_account(account);
    }

    /// Cache-aware sender-row scan for one mailbox (already-acquired session).
    /// STATUS-validates the cache: reuses rows on a hit, fetches only the new
    /// tail on an append, and re-scans otherwise.
    async fn cached_sender_rows(
        &self,
        account: &str,
        mbox: &str,
        session: &mut imap_client::ImapSession,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<Vec<scan_cache::SenderRow>> {
        use scan_cache::CacheDecision;
        let key = (account.to_string(), mbox.to_string());
        let status = imap_client::mailbox_status(session, mbox).await?;

        let cached_meta = self.scan_cache.lock().sender.get(&key).map(|c| c.meta);

        match CacheDecision::from_status(cached_meta.as_ref(), &status) {
            CacheDecision::Hit => Ok(self
                .scan_cache
                .lock()
                .sender
                .get(&key)
                .map(|c| c.rows.clone())
                .unwrap_or_default()),
            CacheDecision::Incremental { from_uid } => {
                let mb = imap_client::select(session, mbox).await?;
                let uids = imap_client::search_uids_from(session, from_uid).await?;
                let new_rows =
                    imap_client::fetch_sender_dates_for_uids(session, &uids, on_progress, cancel)
                        .await?;
                let mut cache = self.scan_cache.lock();
                let entry = cache
                    .sender
                    .entry(key)
                    .or_insert_with(|| scan_cache::CachedScan {
                        meta: scan_cache::CacheMeta {
                            uid_validity: 0,
                            uid_next: 0,
                            exists: 0,
                        },
                        rows: Vec::new(),
                    });
                entry.rows.extend(new_rows);
                if let Some(meta) = mailbox_meta(&mb) {
                    entry.meta = meta;
                }
                Ok(entry.rows.clone())
            }
            CacheDecision::FullRescan => {
                let mb = imap_client::select(session, mbox).await?;
                let uids = imap_client::search_uids(session, "ALL").await?;
                let rows =
                    imap_client::fetch_sender_dates_for_uids(session, &uids, on_progress, cancel)
                        .await?;
                // Only cache when the server gave us validatable metadata.
                if let Some(meta) = mailbox_meta(&mb) {
                    self.scan_cache.lock().sender.insert(
                        key,
                        scan_cache::CachedScan {
                            meta,
                            rows: rows.clone(),
                        },
                    );
                }
                Ok(rows)
            }
        }
    }

    /// Cache-aware list-header scan for one mailbox (already-acquired session).
    /// Backs both `rank_unsubscribe` and `rank_list_id`.
    async fn cached_list_rows(
        &self,
        account: &str,
        mbox: &str,
        session: &mut imap_client::ImapSession,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<Vec<imap_client::ListHeaderRow>> {
        use scan_cache::CacheDecision;
        let key = (account.to_string(), mbox.to_string());
        let status = imap_client::mailbox_status(session, mbox).await?;

        let cached_meta = self.scan_cache.lock().list.get(&key).map(|c| c.meta);

        match CacheDecision::from_status(cached_meta.as_ref(), &status) {
            CacheDecision::Hit => Ok(self
                .scan_cache
                .lock()
                .list
                .get(&key)
                .map(|c| c.rows.clone())
                .unwrap_or_default()),
            CacheDecision::Incremental { from_uid } => {
                let mb = imap_client::select(session, mbox).await?;
                let uids = imap_client::search_uids_from(session, from_uid).await?;
                let new_rows =
                    imap_client::fetch_list_headers_for_uids(session, &uids, on_progress, cancel)
                        .await?;
                let mut cache = self.scan_cache.lock();
                let entry = cache
                    .list
                    .entry(key)
                    .or_insert_with(|| scan_cache::CachedScan {
                        meta: scan_cache::CacheMeta {
                            uid_validity: 0,
                            uid_next: 0,
                            exists: 0,
                        },
                        rows: Vec::new(),
                    });
                entry.rows.extend(new_rows);
                if let Some(meta) = mailbox_meta(&mb) {
                    entry.meta = meta;
                }
                Ok(entry.rows.clone())
            }
            CacheDecision::FullRescan => {
                let mb = imap_client::select(session, mbox).await?;
                let uids = imap_client::search_uids(session, "ALL").await?;
                let rows =
                    imap_client::fetch_list_headers_for_uids(session, &uids, on_progress, cancel)
                        .await?;
                if let Some(meta) = mailbox_meta(&mb) {
                    self.scan_cache.lock().list.insert(
                        key,
                        scan_cache::CachedScan {
                            meta,
                            rows: rows.clone(),
                        },
                    );
                }
                Ok(rows)
            }
        }
    }
}

/// Build cache metadata from a SELECT response, if the server supplied both
/// UIDVALIDITY and UIDNEXT (without them the cache can't be validated later).
fn mailbox_meta(mb: &async_imap::types::Mailbox) -> Option<scan_cache::CacheMeta> {
    Some(scan_cache::CacheMeta {
        uid_validity: mb.uid_validity?,
        uid_next: mb.uid_next?,
        exists: mb.exists,
    })
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// From a set of candidate UIDs, fetch FROM + List-Unsubscribe/Post headers and
/// return only those that match the exact sender AND have either List-Unsubscribe
/// or List-Unsubscribe-Post (i.e. bulk/marketing mail).
async fn filter_sender_bulk_mail(
    session: &mut imap_client::ImapSession,
    candidate_uids: &[u32],
    target_email: &str,
    target_name: &str,
    cancel: Option<&CancelFn>,
) -> Result<Vec<u32>> {
    let mut exact = Vec::new();
    for chunk in candidate_uids.chunks(1000) {
        imap_client::check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetched = imap_client::timed_uid_fetch_collect_pub(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (FROM List-Unsubscribe List-Unsubscribe-Post)])",
        )
        .await?;

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let uid = match fetch.uid {
                Some(u) => u,
                None => continue,
            };
            let header_bytes = fetch.header().unwrap_or(&[]);
            let header_str = String::from_utf8_lossy(header_bytes);

            // Must have List-Unsubscribe OR List-Unsubscribe-Post
            let has_unsub =
                imap_client::extract_header_value_pub(&header_str, "List-Unsubscribe").is_some();
            let has_unsub_post =
                imap_client::extract_header_value_pub(&header_str, "List-Unsubscribe-Post")
                    .is_some();
            if !has_unsub && !has_unsub_post {
                continue;
            }

            // Must match exact sender
            if let Ok((email, name, _, _)) = parser::parse_sender_date(header_bytes)
                && email == target_email
                && name == target_name
            {
                exact.push(uid);
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
/// Attempt RFC 8058 one-click unsubscribe.
async fn attempt_one_click_unsubscribe(
    list_unsubscribe: Option<&str>,
    list_unsubscribe_post: Option<&str>,
) -> UnsubscribeResult {
    let fail = |reason: &str| UnsubscribeResult {
        success: false,
        method: None,
        url: None,
        http_status: None,
        reason: Some(reason.to_string()),
    };

    // RFC 8058 requires both List-Unsubscribe (with https URL) and
    // List-Unsubscribe-Post: List-Unsubscribe=One-Click
    let post_value = match list_unsubscribe_post {
        Some(v) if v.contains("List-Unsubscribe=One-Click") => v,
        _ => {
            return fail(
                "No List-Unsubscribe-Post header with One-Click support. Manual unsubscribe may be required via the List-Unsubscribe URL.",
            );
        }
    };
    let _ = post_value;

    let unsub_header = match list_unsubscribe {
        Some(h) => h,
        None => return fail("No List-Unsubscribe header found."),
    };

    let url = match extract_https_unsubscribe_url(unsub_header) {
        Some(u) => u,
        None => {
            return fail(
                "No HTTPS URL found in List-Unsubscribe header. Only mailto: links are present.",
            );
        }
    };

    // Send the one-click POST request per RFC 8058
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => return fail(&format!("Failed to create HTTP client: {e}")),
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
                UnsubscribeResult {
                    success: true,
                    method: Some("one-click".to_string()),
                    url: Some(url),
                    http_status: Some(status),
                    reason: None,
                }
            } else {
                UnsubscribeResult {
                    success: false,
                    method: None,
                    url: Some(url),
                    http_status: Some(status),
                    reason: Some(format!("Unsubscribe endpoint returned HTTP {status}")),
                }
            }
        }
        Err(e) => UnsubscribeResult {
            success: false,
            method: None,
            url: Some(url),
            http_status: None,
            reason: Some(format!("HTTP request failed: {e}")),
        },
    }
}

/// Clamp a value to a range with a default.
pub fn clamp_usize(val: Option<u64>, default: usize, min: usize, max: usize) -> usize {
    let v = val.map(|v| v as usize).unwrap_or(default);
    v.max(min).min(max)
}

/// Auto-detect the Trash mailbox by scanning LIST results.
/// Auto-detect the Trash mailbox via RFC 6154 `\Trash` role, with string fallback.
/// Resolve the Trash mailbox name from a mailbox listing: RFC 6154 `\Trash`
/// role first, then well-known names, then any name containing "trash" or
/// "deleted". Pure (no I/O) so it can be unit-tested.
fn resolve_trash_from(entries: &[imap_client::MailboxEntry]) -> Option<String> {
    if let Some(entry) = entries.iter().find(|e| e.role.as_deref() == Some("trash")) {
        return Some(entry.name.clone());
    }
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for candidate in [
        "Trash",
        "[Gmail]/Trash",
        "INBOX.Trash",
        "Deleted Messages",
        "Deleted",
    ] {
        if let Some(name) = names.iter().find(|n| n.eq_ignore_ascii_case(candidate)) {
            return Some(name.to_string());
        }
    }
    names
        .iter()
        .find(|n| n.to_lowercase().contains("trash") || n.to_lowercase().contains("deleted"))
        .map(|n| n.to_string())
}

/// Resolve the Drafts mailbox name from a mailbox listing: RFC 6154 `\Drafts`
/// role first, then well-known names, then any name containing "draft". Pure.
fn resolve_drafts_from(entries: &[imap_client::MailboxEntry]) -> Option<String> {
    if let Some(entry) = entries.iter().find(|e| e.role.as_deref() == Some("drafts")) {
        return Some(entry.name.clone());
    }
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for candidate in ["Drafts", "[Gmail]/Drafts", "INBOX.Drafts"] {
        if let Some(name) = names.iter().find(|n| n.eq_ignore_ascii_case(candidate)) {
            return Some(name.to_string());
        }
    }
    names
        .iter()
        .find(|n| n.to_lowercase().contains("draft"))
        .map(|n| n.to_string())
}

/// List mailbox names suitable for scanning — excludes Trash, Junk, Spam, and Drafts.
/// These mailboxes contain deleted/spam/incomplete messages that skew scan results.
async fn list_scannable_mailbox_names(
    session: &mut imap_client::ImapSession,
) -> Result<Vec<String>> {
    let entries = imap_client::list_mailbox_entries(session).await?;

    /// Roles that should be skipped during whole-account scans.
    const SKIP_ROLES: &[&str] = &["trash", "junk", "drafts", "all"];

    Ok(entries
        .into_iter()
        .filter(|entry| {
            // Skip non-selectable mailboxes (\NoSelect).
            if entry.no_select {
                return false;
            }

            // Skip mailboxes with a skip-listed RFC 6154 role.
            if let Some(ref role) = entry.role
                && SKIP_ROLES.contains(&role.as_str())
            {
                return false;
            }

            // Fallback: string-based filtering for servers that don't send
            // RFC 6154 special-use attributes.
            if entry.role.is_none() && is_skippable_scan_name(&entry.name.to_lowercase()) {
                return false;
            }

            true
        })
        .map(|entry| entry.name)
        .collect())
}

/// True for mailbox names that should be skipped during whole-account scans on
/// servers that don't advertise RFC 6154 special-use attributes — Junk/Spam/
/// Trash/Drafts plus Gmail-style "All Mail" (the union folder, which would
/// otherwise double-count every message). `lower` must be lowercased.
fn is_skippable_scan_name(lower: &str) -> bool {
    lower.contains("junk")
        || lower.contains("spam")
        || lower.contains("trash")
        || lower.contains("deleted")
        || lower.contains("draft")
        || is_all_mail_name(lower)
}

/// Tight match for an "All Mail" union folder (Gmail and similar). Deliberately
/// not a bare `contains("all")`, which would wrongly skip user folders such as
/// "All Hands" or "Marketing/All Staff". `lower` must be lowercased.
fn is_all_mail_name(lower: &str) -> bool {
    lower == "all mail" || lower == "[gmail]/all mail" || lower.ends_with("/all mail")
}

/// Whether a message's `List-Id` header value matches the requested List-Id.
/// IMAP `HEADER` search is substring-only, so `delete_list_id` confirms the
/// exact list here before deleting. Compared case-insensitively after trimming;
/// the value round-trips exactly from `rank_list_id`'s `listId` output.
fn list_id_matches(requested: &str, candidate: &str) -> bool {
    requested.trim().eq_ignore_ascii_case(candidate.trim())
}

/// Extract the display name from a List-Id header value.
/// Format: `Cool List <cool.example.com>` → "Cool List"
/// If no display name, returns the identifier: `<cool.example.com>` → "cool.example.com"
fn extract_list_id_display(list_id: &str) -> String {
    let trimmed = list_id.trim();
    if let Some(bracket_start) = trimmed.find('<') {
        let before = trimmed[..bracket_start].trim();
        if !before.is_empty() {
            return before.to_string();
        }
        // No display name — extract the identifier from angle brackets
        if let Some(bracket_end) = trimmed.find('>') {
            return trimmed[bracket_start + 1..bracket_end].to_string();
        }
    }
    trimmed.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, role: Option<&str>) -> imap_client::MailboxEntry {
        imap_client::MailboxEntry {
            name: name.to_string(),
            no_select: false,
            role: role.map(String::from),
        }
    }

    #[test]
    fn resolve_trash_prefers_rfc6154_role_over_name() {
        let entries = [
            entry("INBOX", None),
            entry("Bin", Some("trash")),
            entry("Trash", None), // would match by name, but role wins
        ];
        assert_eq!(resolve_trash_from(&entries), Some("Bin".to_string()));
    }

    #[test]
    fn resolve_trash_falls_back_to_known_names() {
        let gmail = [entry("INBOX", None), entry("[Gmail]/Trash", None)];
        assert_eq!(
            resolve_trash_from(&gmail),
            Some("[Gmail]/Trash".to_string())
        );

        let dovecot = [entry("INBOX", None), entry("INBOX.Trash", None)];
        assert_eq!(
            resolve_trash_from(&dovecot),
            Some("INBOX.Trash".to_string())
        );
    }

    #[test]
    fn resolve_trash_substring_last_resort_and_none() {
        let weird = [entry("INBOX", None), entry("Papierkorb Deleted", None)];
        assert_eq!(
            resolve_trash_from(&weird),
            Some("Papierkorb Deleted".to_string())
        );
        assert_eq!(resolve_trash_from(&[entry("INBOX", None)]), None);
    }

    #[test]
    fn resolve_drafts_prefers_role_then_name() {
        let role = [entry("INBOX", None), entry("My Drafts", Some("drafts"))];
        assert_eq!(resolve_drafts_from(&role), Some("My Drafts".to_string()));

        let named = [entry("INBOX", None), entry("[Gmail]/Drafts", None)];
        assert_eq!(
            resolve_drafts_from(&named),
            Some("[Gmail]/Drafts".to_string())
        );
        assert_eq!(resolve_drafts_from(&[entry("INBOX", None)]), None);
    }

    #[test]
    fn all_mail_name_matches_union_folders_only() {
        assert!(is_all_mail_name("all mail"));
        assert!(is_all_mail_name("[gmail]/all mail"));
        assert!(is_all_mail_name("[google mail]/all mail"));
        // Not all-mail: ordinary user folders with "all" in the name.
        assert!(!is_all_mail_name("all hands"));
        assert!(!is_all_mail_name("marketing/all staff"));
        assert!(!is_all_mail_name("inbox"));
    }

    #[test]
    fn list_id_matches_is_exact_not_substring() {
        assert!(list_id_matches("news.example.com", "news.example.com"));
        assert!(list_id_matches(
            "Cool List <cool.example.com>",
            "cool list <cool.example.com>"
        )); // case-insensitive, trimmed
        assert!(list_id_matches("news.example.com", "  news.example.com  "));
        // Substring must NOT match — this is the over-deletion guard.
        assert!(!list_id_matches("news", "newsletter.example.com"));
        assert!(!list_id_matches("list.example.com", "sub.list.example.com"));
    }

    #[test]
    fn skippable_scan_name_covers_special_folders_and_all_mail() {
        for skip in [
            "junk",
            "spam",
            "deleted items",
            "[gmail]/trash",
            "drafts",
            "[gmail]/all mail",
        ] {
            assert!(is_skippable_scan_name(skip), "should skip {skip:?}");
        }
        for keep in ["inbox", "work", "receipts", "all hands"] {
            assert!(!is_skippable_scan_name(keep), "should keep {keep:?}");
        }
    }

    /// Fast test for the early validation error in create_draft.
    /// Uses an empty config so no IMAP connection or credentials are needed.
    /// This exercises the public API path that the MCP tool also goes through.
    #[tokio::test]
    async fn create_draft_rejects_empty_recipients() {
        let mk = Agentmail::new(Config::empty());

        let err = mk
            .create_draft("any-account", "subj", "body", &[], &[], &[], &[])
            .await
            .expect_err("should fail with no recipients");

        let msg = err.to_string();
        assert!(
            msg.contains("recipient"),
            "expected recipient validation error, got: {msg}"
        );
    }

    /// Sanity check that we can construct an Agentmail and call create_draft
    /// with attachments (it will fail at the "account not found" stage, which
    /// proves the attachments parameter is accepted and passed down).
    #[tokio::test]
    async fn create_draft_accepts_attachments_before_account_lookup() {
        let mk = Agentmail::new(Config::empty());

        let att = DraftAttachment {
            filename: "test.txt".to_string(),
            content_type: "text/plain".to_string(),
            data: b"hello attachment".to_vec(),
        };

        let err = mk
            .create_draft(
                "nonexistent-account",
                "with att",
                "body",
                &["to@example.com".to_string()],
                &[],
                &[],
                &[att],
            )
            .await
            .expect_err("should fail because account doesn't exist");

        // The important thing is we didn't fail earlier (e.g. on attachment handling).
        // The actual error should be about the missing account.
        let msg = err.to_string();
        assert!(
            msg.contains("Account") || msg.contains("not found"),
            "expected account lookup error, got: {msg}"
        );
    }
}
