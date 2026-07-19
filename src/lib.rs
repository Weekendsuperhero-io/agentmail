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

mod header_cache;
mod mailbox_catalog;
mod scan_plan;
mod unsubscribe;

pub use config::{AccountConfig, Config};
pub use connection::ConnectionPool;
pub use error::{AgentmailError, Result};
pub use imap_client::{CancelFn, ProgressFn};
pub use provider::MailProvider;
pub use secret::init_service_name;
pub use types::*;

/// High-level facade for IMAP operations.
/// Owns the connection pool and configuration.
pub struct Agentmail {
    pool: ConnectionPool,
    /// Per-account mailbox hierarchy used by completion and special-use lookup.
    mailbox_catalog: mailbox_catalog::MailboxCatalog,
    /// Persistent UID membership and immutable ranking-header projection.
    header_cache: header_cache::HeaderCache,
}

impl Agentmail {
    /// Create from an existing config.
    pub fn new(config: Config) -> Self {
        Self {
            pool: ConnectionPool::new(config),
            mailbox_catalog: mailbox_catalog::MailboxCatalog::default(),
            header_cache: header_cache::HeaderCache::default(),
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

    /// The account's own email address(es), lowercased. Used to exclude the
    /// user's own sent mail from sender rankings ("skip myself as a sender")
    /// without hiding the Sent folder from other tools.
    fn own_addresses(&self, account: &str) -> hashbrown::HashSet<String> {
        let mut set = hashbrown::HashSet::new();
        if let Some(cfg) = self.pool.account_config(account) {
            set.insert(cfg.username.to_lowercase());
        }
        set
    }

    fn validate_uid_selector(
        mailbox: &str,
        expected_uid_validity: u32,
        uids: &[u32],
    ) -> Result<()> {
        if expected_uid_validity == 0 {
            return Err(AgentmailError::UidValidityChanged {
                mailbox: mailbox.to_string(),
                expected: expected_uid_validity,
                actual: None,
            });
        }
        if uids.contains(&0) {
            return Err(AgentmailError::MessageNotFound(0));
        }
        Ok(())
    }

    async fn live_ranking_headers(
        &self,
        session: &mut imap_client::ImapSession,
        mailboxes: &[String],
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<Vec<(String, imap_client::ListHeaderRow)>> {
        let mut rows = Vec::new();
        for mailbox in mailboxes {
            imap_client::check_cancel(cancel)?;
            rows.extend(
                imap_client::fetch_rank_headers(session, mailbox, on_progress, cancel)
                    .await?
                    .into_iter()
                    .map(|row| (mailbox.clone(), row)),
            );
        }
        rows.sort_unstable_by(|(mailbox_a, row_a), (mailbox_b, row_b)| {
            row_b
                .date
                .cmp(&row_a.date)
                .then_with(|| mailbox_b.cmp(mailbox_a))
                .then_with(|| row_b.uid.cmp(&row_a.uid))
        });
        Ok(rows)
    }

    async fn fence_header_cache_mutation(&self, account: &str) {
        if let Some(config) = self.pool.account_config(account) {
            self.header_cache
                .fence_account_mutation(account, config)
                .await;
        }
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
    ///
    /// Probe contract: a parameter error (unknown account) is an `Err`, while
    /// connectivity and auth outcomes are data — `connected: false` plus the
    /// error text. A probe against a configured account never raises for an
    /// unreachable or rejecting server.
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
            Err(e @ AgentmailError::AccountNotFound(_)) => Err(e),
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

    /// List one bounded page of selectable mailboxes for a required account.
    /// STATUS is requested only for rows in the returned page.
    pub async fn list_mailboxes_page(
        &self,
        account: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(ListMailboxesResponse, usize)> {
        if !self.pool.config().accounts.contains_key(account) {
            return Err(AgentmailError::AccountNotFound(account.to_string()));
        }

        let caps = self.caps_for(account).await?;
        let account_name = account.to_string();
        let (mailboxes, total) = self
            .pool
            .with_session_retry(account, async move |session| {
                imap_client::list_mailboxes_page(session, &account_name, &caps, offset, limit).await
            })
            .await?;
        Ok((ListMailboxesResponse { mailboxes }, total))
    }

    /// Return the short-lived mailbox hierarchy snapshot used for completion.
    pub(crate) async fn cached_mailbox_layout(
        &self,
        account: &str,
    ) -> Result<std::sync::Arc<[imap_client::MailboxLayout]>> {
        if !self.pool.config().accounts.contains_key(account) {
            return Err(AgentmailError::AccountNotFound(account.to_string()));
        }
        if let Some(entries) = self.mailbox_catalog.get(account) {
            return Ok(entries);
        }

        // Every refresher acquires its pool session before the per-account
        // refresh gate. Keeping that order consistent prevents lock inversion
        // with callers that already hold a session for the surrounding action.
        let mut session = self.pool.acquire(account).await?;
        let result = self
            .mailbox_catalog
            .get_or_refresh(account, || {
                imap_client::list_mailbox_layout(session.session())
            })
            .await;
        session.release().await;
        result
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
            self.invalidate_mailbox_catalog(account);
            return Ok(CreateMailboxResponse {
                account: account.to_string(),
                mailbox: mailbox_name.to_string(),
                created: false,
                already_exists: true,
            });
        }

        // Invalidate before CREATE: the server can apply the command even when
        // the tagged response is lost and the client observes an error.
        self.invalidate_mailbox_catalog(account);
        let create_result = imap_client::create_mailbox(session.session(), mailbox_name).await;
        // Fence a catalog refresh that raced the server-side CREATE.
        self.invalidate_mailbox_catalog(account);
        create_result?;
        imap_client::sync(session.session()).await?;
        session.release().await;

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
        let (messages, total, uid_validity) = self
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
            uid_validity,
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
        expected_uid_validity: u32,
        include_content: bool,
        include_headers: bool,
    ) -> Result<GetMessagesByUidResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, uids)?;
        let (mailbox_s, account_s, uids_v) =
            (mailbox.to_string(), account.to_string(), uids.to_vec());
        let messages = self
            .pool
            .with_session_retry(account, async move |s| {
                imap_client::examine_with_expected_uid_validity(
                    s,
                    &mailbox_s,
                    expected_uid_validity,
                )
                .await?;
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
            uid_validity: expected_uid_validity,
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
        let (messages, total, uid_validity) = self
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
            uid_validity,
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
    pub async fn top_senders(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopSendersResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => {
                self.account_scan_mailboxes(
                    account,
                    session.session(),
                    scan_plan::ScanPurpose::Discovery,
                )
                .await?
            }
        };

        let config = self
            .pool
            .account_config(account)
            .ok_or_else(|| AgentmailError::AccountNotFound(account.to_string()))?;
        let own = self.own_addresses(account);
        let own_vec: Vec<String> = own.iter().cloned().collect();
        if let Some(page) = self
            .header_cache
            .top_senders_page(
                session.session(),
                account,
                config,
                &mailboxes,
                &own_vec,
                offset,
                limit,
                on_progress,
                cancel,
            )
            .await?
        {
            session.release().await;
            let item_count = page.items.len();
            let senders = page
                .items
                .into_iter()
                .map(|row| SenderSummary {
                    sender: if row.display_name.is_empty() {
                        row.address.clone()
                    } else {
                        format!("{} <{}>", row.display_name, row.address)
                    },
                    address: row.address,
                    display_name: row.display_name,
                    sample: MailboxMessageIdentity {
                        mailbox: row.sample.mailbox,
                        uid_validity: row.sample.uid_validity,
                        uid: row.sample.uid,
                    },
                    count: u32::try_from(row.count).unwrap_or(u32::MAX),
                    oldest_date: row.oldest_date,
                    newest_date: row.newest_date,
                })
                .collect();
            let unique_senders = usize::try_from(page.total_groups).unwrap_or(usize::MAX);
            return Ok(TopSendersResponse {
                mailbox: mailbox.unwrap_or("*").to_string(),
                account: account.to_string(),
                total_messages: u32::try_from(page.total_messages).unwrap_or(u32::MAX),
                unique_senders,
                offset,
                limit,
                next_offset: next_offset(offset, item_count, unique_senders),
                senders,
            });
        }

        use hashbrown::{HashMap, HashSet};
        // Key by (email, display_name) so "Find My <noreply@apple.com>" and
        // "iCloud <noreply@apple.com>" are separate entries.
        let mut map: HashMap<(String, String), SenderSummary> = HashMap::new();
        // Dedup the same logical message across folders (Gmail labels / All Mail).
        let mut seen: HashSet<String> = HashSet::new();
        // Don't rank the user themselves (their own sent mail).

        let live_rows = self
            .live_ranking_headers(session.session(), &mailboxes, on_progress, cancel)
            .await?;
        for (mbox, row) in live_rows {
            if row.sender_email.is_empty() || own.contains(&row.sender_email) {
                continue;
            }
            if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                continue; // already counted this message from another folder
            }
            let key = (row.sender_email.clone(), row.sender_name.clone());
            let uid_validity =
                row.uid_validity
                    .ok_or_else(|| AgentmailError::UidValidityUnavailable {
                        mailbox: mbox.clone(),
                    })?;
            let entry = map.entry(key).or_insert_with(|| SenderSummary {
                sender: String::new(),
                address: row.sender_email.clone(),
                display_name: row.sender_name.clone(),
                sample: MailboxMessageIdentity {
                    mailbox: mbox.clone(),
                    uid_validity,
                    uid: row.uid,
                },
                count: 0,
                oldest_date: None,
                newest_date: None,
            });

            entry.count += 1;

            if ranking_sample_is_newer(
                (row.date, &mbox, row.uid),
                (
                    entry.newest_date,
                    Some(entry.sample.mailbox.as_str()),
                    entry.sample.uid,
                ),
                entry.count == 1,
            ) {
                entry.sample = MailboxMessageIdentity {
                    mailbox: mbox.clone(),
                    uid_validity,
                    uid: row.uid,
                };
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
        senders.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.address.cmp(&b.address))
                .then_with(|| a.display_name.cmp(&b.display_name))
        });

        let unique_senders = senders.len();
        let total_messages = senders.iter().map(|s| s.count).sum::<u32>();
        let senders: Vec<_> = senders.into_iter().skip(offset).take(limit).collect();
        let item_count = senders.len();

        Ok(TopSendersResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_senders,
            offset,
            limit,
            next_offset: next_offset(offset, item_count, unique_senders),
            senders,
        })
    }

    /// Group mailing-list messages by sender.
    ///
    /// Includes messages that have List-Unsubscribe or List-Unsubscribe-Post.
    /// Groups by exact sender (email + display name). The sample_uid and
    /// unsubscribe info come from the newest message in each group.
    ///
    /// When `mailbox` is `None`, scans all mailboxes in the account.
    pub async fn top_subscriptions(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopSubscriptionsResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => {
                self.account_scan_mailboxes(
                    account,
                    session.session(),
                    scan_plan::ScanPurpose::Discovery,
                )
                .await?
            }
        };

        let config = self
            .pool
            .account_config(account)
            .ok_or_else(|| AgentmailError::AccountNotFound(account.to_string()))?;
        let own = self.own_addresses(account);
        let own_vec: Vec<String> = own.iter().cloned().collect();
        if let Some(page) = self
            .header_cache
            .top_subscriptions_page(
                session.session(),
                account,
                config,
                &mailboxes,
                &own_vec,
                offset,
                limit,
                on_progress,
                cancel,
            )
            .await?
        {
            session.release().await;
            let item_count = page.items.len();
            let lists = page
                .items
                .into_iter()
                .map(|row| ListSummary {
                    sender: if row.display_name.is_empty() {
                        row.address.clone()
                    } else {
                        format!("{} <{}>", row.display_name, row.address)
                    },
                    address: row.address,
                    display_name: row.display_name,
                    advertised_one_click: row.advertised_one_click,
                    sample: MailboxMessageIdentity {
                        mailbox: row.sample.mailbox,
                        uid_validity: row.sample.uid_validity,
                        uid: row.sample.uid,
                    },
                    count: u32::try_from(row.count).unwrap_or(u32::MAX),
                    oldest_date: row.oldest_date,
                    newest_date: row.newest_date,
                })
                .collect();
            let unique_lists = usize::try_from(page.total_groups).unwrap_or(usize::MAX);
            return Ok(TopSubscriptionsResponse {
                mailbox: mailbox.unwrap_or("*").to_string(),
                account: account.to_string(),
                total_messages: u32::try_from(page.total_messages).unwrap_or(u32::MAX),
                unique_lists,
                offset,
                limit,
                next_offset: next_offset(offset, item_count, unique_lists),
                lists,
            });
        }

        use hashbrown::{HashMap, HashSet};
        use types::ListSummary;

        // Key by (email, display_name) for exact sender grouping
        let mut map: HashMap<(String, String), ListSummary> = HashMap::new();
        // Dedup the same logical message across folders (Gmail labels / All Mail).
        let mut seen: HashSet<String> = HashSet::new();
        // Don't rank the user themselves (their own sent mail).

        let live_rows = self
            .live_ranking_headers(session.session(), &mailboxes, on_progress, cancel)
            .await?;
        for (mbox, row) in live_rows {
            if (row.list_unsubscribe.is_none() && row.list_unsubscribe_post.is_none())
                || row.sender_email.is_empty()
                || own.contains(&row.sender_email)
            {
                continue;
            }
            if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                continue; // already counted this message from another folder
            }
            let key = (row.sender_email.clone(), row.sender_name.clone());
            let uid_validity =
                row.uid_validity
                    .ok_or_else(|| AgentmailError::UidValidityUnavailable {
                        mailbox: mbox.clone(),
                    })?;
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
                    advertised_one_click: false,
                    sample: MailboxMessageIdentity {
                        mailbox: mbox.clone(),
                        uid_validity,
                        uid: row.uid,
                    },
                    count: 0,
                    oldest_date: None,
                    newest_date: None,
                }
            });

            entry.count += 1;

            // Track the newest message — its UID and unsubscribe info are used
            let is_newer = ranking_sample_is_newer(
                (row.date, &mbox, row.uid),
                (
                    entry.newest_date,
                    Some(entry.sample.mailbox.as_str()),
                    entry.sample.uid,
                ),
                entry.count == 1,
            );

            if is_newer {
                entry.sample = MailboxMessageIdentity {
                    mailbox: mbox.clone(),
                    uid_validity,
                    uid: row.uid,
                };
                entry.advertised_one_click = unsubscribe::advertises_one_click(
                    row.list_unsubscribe.as_deref(),
                    row.list_unsubscribe_post.as_deref(),
                );
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

        imap_client::check_cancel(cancel)?;
        session.release().await;

        let mut lists: Vec<ListSummary> = map.into_values().collect();
        // One-click senders first, then by message count
        lists.sort_by(|a, b| {
            b.advertised_one_click
                .cmp(&a.advertised_one_click)
                .then_with(|| b.count.cmp(&a.count))
                .then_with(|| a.address.cmp(&b.address))
                .then_with(|| a.display_name.cmp(&b.display_name))
        });

        let unique_lists = lists.len();
        let total_messages = lists.iter().map(|l| l.count).sum::<u32>();
        let lists: Vec<_> = lists.into_iter().skip(offset).take(limit).collect();
        let item_count = lists.len();

        Ok(TopSubscriptionsResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_lists,
            offset,
            limit,
            next_offset: next_offset(offset, item_count, unique_lists),
            lists,
        })
    }

    /// Group messages by List-Id header (RFC 2919).
    ///
    /// Groups all messages from the same mailing list regardless of sender.
    /// When `mailbox` is `None`, scans all mailboxes (excluding trash/junk/drafts).
    pub async fn top_mailing_lists(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopMailingListsResponse> {
        let mut session = self.pool.acquire(account).await?;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => {
                self.account_scan_mailboxes(
                    account,
                    session.session(),
                    scan_plan::ScanPurpose::Discovery,
                )
                .await?
            }
        };

        let config = self
            .pool
            .account_config(account)
            .ok_or_else(|| AgentmailError::AccountNotFound(account.to_string()))?;
        if let Some(page) = self
            .header_cache
            .top_mailing_lists_page(
                session.session(),
                account,
                config,
                &mailboxes,
                offset,
                limit,
                on_progress,
                cancel,
            )
            .await?
        {
            session.release().await;
            let item_count = page.items.len();
            let lists = page
                .items
                .into_iter()
                .map(|row| ListIdSummary {
                    list_id: row.list_id,
                    display_name: row.display_name,
                    senders: row.senders,
                    sender_count: usize::try_from(row.sender_count).unwrap_or(usize::MAX),
                    count: u32::try_from(row.count).unwrap_or(u32::MAX),
                    sample: MailboxMessageIdentity {
                        mailbox: row.sample.mailbox,
                        uid_validity: row.sample.uid_validity,
                        uid: row.sample.uid,
                    },
                    oldest_date: row.oldest_date,
                    newest_date: row.newest_date,
                })
                .collect();
            let unique_lists = usize::try_from(page.total_groups).unwrap_or(usize::MAX);
            return Ok(TopMailingListsResponse {
                mailbox: mailbox.unwrap_or("*").to_string(),
                account: account.to_string(),
                total_messages: u32::try_from(page.total_messages).unwrap_or(u32::MAX),
                unique_lists,
                offset,
                limit,
                next_offset: next_offset(offset, item_count, unique_lists),
                lists,
            });
        }

        use hashbrown::{HashMap, HashSet};

        struct ListIdEntry {
            display_name: String,
            senders: HashSet<String>,
            count: u32,
            sample_uid: u32,
            sample_uid_validity: u32,
            sample_mailbox: String,
            oldest_date: Option<chrono::DateTime<chrono::Utc>>,
            newest_date: Option<chrono::DateTime<chrono::Utc>>,
        }

        let mut map: HashMap<String, ListIdEntry> = HashMap::new();
        // Dedup the same logical message across folders (Gmail labels / All Mail).
        let mut seen: HashSet<String> = HashSet::new();

        let live_rows = self
            .live_ranking_headers(session.session(), &mailboxes, on_progress, cancel)
            .await?;
        for (mbox, row) in live_rows {
            let raw_list_id = match row.list_id {
                Some(ref id) if !id.is_empty() => id.clone(),
                _ => continue, // Skip messages without List-Id
            };
            let Some(list_id) = normalize_list_id(&raw_list_id) else {
                continue;
            };
            if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                continue; // already counted this message from another folder
            }

            let uid_validity =
                row.uid_validity
                    .ok_or_else(|| AgentmailError::UidValidityUnavailable {
                        mailbox: mbox.clone(),
                    })?;

            let entry = map.entry(list_id.clone()).or_insert_with(|| {
                let display = extract_list_id_display(&raw_list_id);
                ListIdEntry {
                    display_name: display,
                    senders: HashSet::new(),
                    count: 0,
                    sample_uid: row.uid,
                    sample_uid_validity: uid_validity,
                    sample_mailbox: mbox.clone(),
                    oldest_date: None,
                    newest_date: None,
                }
            });

            entry.count += 1;
            if !row.sender_email.is_empty() {
                entry.senders.insert(row.sender_email.clone());
            }

            let is_newer = ranking_sample_is_newer(
                (row.date, &mbox, row.uid),
                (
                    entry.newest_date,
                    Some(entry.sample_mailbox.as_str()),
                    entry.sample_uid,
                ),
                entry.count == 1,
            );
            if is_newer {
                entry.sample_uid = row.uid;
                entry.sample_uid_validity = uid_validity;
                entry.sample_mailbox = mbox.clone();
                entry.display_name = extract_list_id_display(&raw_list_id);
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

        imap_client::check_cancel(cancel)?;
        session.release().await;

        let mut lists: Vec<ListIdSummary> = map
            .into_iter()
            .map(|(list_id, entry)| {
                let mut senders: Vec<String> = entry.senders.into_iter().collect();
                senders.sort();
                let sender_count = senders.len();
                senders.truncate(5);
                ListIdSummary {
                    list_id,
                    display_name: entry.display_name,
                    senders,
                    sender_count,
                    count: entry.count,
                    sample: MailboxMessageIdentity {
                        mailbox: entry.sample_mailbox,
                        uid_validity: entry.sample_uid_validity,
                        uid: entry.sample_uid,
                    },
                    oldest_date: entry.oldest_date,
                    newest_date: entry.newest_date,
                }
            })
            .collect();
        lists.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.list_id.cmp(&b.list_id))
        });

        let unique_lists = lists.len();
        let total_messages = lists.iter().map(|l| l.count).sum::<u32>();
        let lists: Vec<_> = lists.into_iter().skip(offset).take(limit).collect();
        let item_count = lists.len();

        Ok(TopMailingListsResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_lists,
            offset,
            limit,
            next_offset: next_offset(offset, item_count, unique_lists),
            lists,
        })
    }

    /// Delete all messages with a specific List-Id across mailboxes.
    /// Upper bound on repeated search→delete passes per mailbox. Windowed
    /// servers (Yahoo/AOL pin the visible mailbox to its newest N messages)
    /// backfill older mail as matches are deleted, so one pass only clears
    /// the current window; the cap is a runaway guard, far above any real
    /// drain. Hitting it reports the mailbox as skipped (incomplete).
    const MAX_WINDOW_DRAIN_PASSES: usize = 500;

    /// Whether this account's server was observed filtering List-* headers
    /// out of HEADER.FIELDS responses (AOL/Yahoo). Such servers also cannot
    /// match those headers in SEARCH, so an empty `HEADER List-Id` result
    /// there means "blind", not "absent". The flag is set by ranking scans —
    /// the workflow that produces a listId always runs one first.
    fn list_search_untrusted(&self, account: &str) -> bool {
        self.pool
            .account_config(account)
            .is_some_and(|config| self.header_cache.account_flagged_quirky(account, config))
    }

    /// Exact-List-Id candidate UIDs in the currently selected mailbox.
    /// Server-side `SEARCH HEADER List-Id` first; when that returns nothing
    /// on a server whose List-* handling is untrusted, enumerate the visible
    /// mailbox and confirm locally — the List-Id confirm fetch works
    /// everywhere because List-Id survives the servers' header filtering.
    async fn exact_list_id_uids(
        &self,
        session: &mut imap_client::ImapSession,
        account: &str,
        list_id: &str,
        mailbox_exists: u32,
        mailbox_uid_next: Option<u32>,
        cancel: Option<&CancelFn>,
    ) -> Result<Vec<u32>> {
        let criteria = SearchCriteria {
            header: Some(("List-Id".to_string(), list_id.to_string())),
            deleted: Some(false),
            ..Default::default()
        };
        let query = imap_client::build_search_query_pub(&criteria)?;
        let mut candidates = imap_client::search_uids(session, &query).await?;
        if candidates.is_empty() && mailbox_exists > 0 && self.list_search_untrusted(account) {
            candidates =
                imap_client::search_all_uids_checked(session, mailbox_exists, mailbox_uid_next)
                    .await?;
        }
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // IMAP HEADER search is substring-only (and the enumeration fallback
        // is everything), so confirm the exact List-Id before deleting.
        Ok(
            imap_client::fetch_list_ids_for_uids_cancellable(session, &candidates, cancel)
                .await?
                .into_iter()
                .filter(|(_, id)| id.as_deref().is_some_and(|v| list_id_matches(list_id, v)))
                .map(|(uid, _)| uid)
                .collect(),
        )
    }

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
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await;

        let mailboxes = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => {
                self.account_scan_mailboxes(
                    account,
                    session.session(),
                    scan_plan::ScanPurpose::Mutation,
                )
                .await?
            }
        };

        let mut total_found = 0usize;
        let mut total_deleted = 0usize;
        let mut total_failed = 0usize;
        let mut per_mailbox = Vec::new();
        let mut skipped = Vec::new();
        let mut cache_dirtied = false;

        for mbox in &mailboxes {
            let mut mailbox_found = 0usize;
            let mut mailbox_deleted = 0usize;
            let mut mailbox_failed = 0usize;
            let mut drained = false;
            for _pass in 0..Self::MAX_WINDOW_DRAIN_PASSES {
                imap_client::check_cancel(cancel)?;
                // Re-select each pass so a windowed server's freshly
                // backfilled messages become visible.
                let mb = match imap_client::select(session.session(), mbox).await {
                    Ok(mb) => mb,
                    Err(_) => {
                        skipped.push(mbox.clone());
                        drained = true;
                        break;
                    }
                };
                // A discovery failure marks the mailbox skipped (coverage
                // incomplete) rather than aborting the account-wide delete.
                let uids = match self
                    .exact_list_id_uids(
                        session.session(),
                        account,
                        list_id,
                        mb.exists,
                        mb.uid_next,
                        cancel,
                    )
                    .await
                {
                    Ok(uids) => uids,
                    Err(_) => {
                        skipped.push(mbox.clone());
                        drained = true;
                        break;
                    }
                };
                if uids.is_empty() {
                    drained = true;
                    break;
                }

                if !cache_dirtied {
                    self.fence_header_cache_mutation(account).await;
                    cache_dirtied = true;
                }
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

                mailbox_found += uids.len();
                mailbox_deleted += result.deleted.len();
                mailbox_failed += result.failed.len();
                if result.deleted.is_empty() {
                    drained = true;
                    break;
                }
            }
            if !drained {
                skipped.push(mbox.clone());
            }

            total_found += mailbox_found;
            total_deleted += mailbox_deleted;
            total_failed += mailbox_failed;
            if mailbox_found > 0 {
                per_mailbox.push(PerMailboxDeleteResult {
                    mailbox: mbox.clone(),
                    found: mailbox_found,
                    deleted: mailbox_deleted,
                    failed: mailbox_failed,
                });
            }
        }

        session.release().await;

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
            None => {
                self.account_scan_mailboxes(
                    account,
                    session.session(),
                    scan_plan::ScanPurpose::Discovery,
                )
                .await?
            }
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
                Err(error) if mailbox.is_none() => {
                    tracing::warn!(
                        target: "agentmail",
                        mailbox = mbox,
                        error = %error,
                        "account-wide flag scan skipped a mailbox"
                    );
                    continue;
                }
                Err(error) => return Err(error),
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
        expected_uid_validity: u32,
        flags: &[String],
        color: Option<&str>,
    ) -> Result<UpdateFlagsResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        let mut session = self.pool.acquire(account).await?;
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;

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
        expected_uid_validity: u32,
        flags: &[String],
        remove_color: bool,
    ) -> Result<UpdateFlagsResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        let mut session = self.pool.acquire(account).await?;
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;

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
            None => {
                self.account_scan_mailboxes(
                    account,
                    session.session(),
                    scan_plan::ScanPurpose::Discovery,
                )
                .await?
            }
        };

        let mut all_messages = Vec::new();
        let mut per_mailbox = Vec::new();

        for mbox in &mailboxes {
            imap_client::check_cancel(cancel)?;
            let (hits, uid_validity) = match imap_client::fetch_attachment_uids(
                session.session(),
                mbox,
                on_progress,
                cancel,
            )
            .await
            {
                Ok(result) => result,
                Err(error) if mailbox.is_none() => {
                    tracing::warn!(
                        target: "agentmail",
                        mailbox = mbox,
                        error = %error,
                        "account-wide attachment scan skipped a mailbox"
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };

            if !hits.is_empty() {
                per_mailbox.push(MailboxAttachmentCount {
                    mailbox: mbox.clone(),
                    count: hits.len(),
                });
                all_messages.extend(hits.into_iter().map(|hit| AttachmentMessage {
                    mailbox: mbox.clone(),
                    uid_validity,
                    uid: hit.uid,
                    date: hit.date,
                }));
            }
        }

        imap_client::check_cancel(cancel)?;
        session.release().await;

        all_messages.sort_unstable_by(|a, b| {
            b.date
                .cmp(&a.date)
                .then_with(|| b.mailbox.cmp(&a.mailbox))
                .then_with(|| b.uid.cmp(&a.uid))
        });

        let total = all_messages.len();
        let messages = all_messages.into_iter().skip(offset).take(limit).collect();

        Ok(FindAttachmentsResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total,
            offset,
            limit,
            messages,
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
        expected_uid_validity: u32,
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<DeleteMessagesResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, uids)?;
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await;
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;
        if !uids.is_empty() {
            self.fence_header_cache_mutation(account).await;
        }
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
        expected_uid_validity: u32,
        all_mailboxes: bool,
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<DeleteBySenderResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await;
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;

        // 1. Fetch the exact sender from the target message
        let (target_email, target_name) = imap_client::fetch_sender(session.session(), uid).await?;

        let sender_display = if target_name.is_empty() {
            target_email.clone()
        } else {
            format!("{} <{}>", target_name, target_email)
        };

        let search_mailboxes = if all_mailboxes {
            self.account_scan_mailboxes(
                account,
                session.session(),
                scan_plan::ScanPurpose::Mutation,
            )
            .await?
        } else {
            vec![mailbox.to_string()]
        };

        let mut total_found = 0usize;
        let mut total_deleted = 0usize;
        let mut total_failed = 0usize;
        let mut per_mailbox = Vec::new();
        let mut skipped = Vec::new();
        let mut cache_dirtied = false;

        for mbox in &search_mailboxes {
            let mut mailbox_found = 0usize;
            let mut mailbox_deleted = 0usize;
            let mut mailbox_failed = 0usize;
            let mut drained = false;
            for _pass in 0..Self::MAX_WINDOW_DRAIN_PASSES {
                imap_client::check_cancel(cancel)?;
                // Re-select each pass so a windowed server's freshly
                // backfilled messages become visible.
                if imap_client::select(session.session(), mbox).await.is_err() {
                    skipped.push(mbox.clone());
                    drained = true;
                    break;
                }

                // Server-side FROM search (substring) to get candidates
                let criteria = SearchCriteria {
                    from: Some(target_email.clone()),
                    deleted: Some(false),
                    ..Default::default()
                };
                let query = imap_client::build_search_query_pub(&criteria)?;
                let candidate_uids = match imap_client::search_uids(session.session(), &query).await
                {
                    Ok(uids) => uids,
                    Err(_) => {
                        skipped.push(mbox.clone());
                        drained = true;
                        break;
                    }
                };

                if candidate_uids.is_empty() {
                    drained = true;
                    break;
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
                    drained = true;
                    break;
                }

                if !cache_dirtied {
                    self.fence_header_cache_mutation(account).await;
                    cache_dirtied = true;
                }
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

                mailbox_found += exact_uids.len();
                mailbox_deleted += result.deleted.len();
                mailbox_failed += result.failed.len();
                if result.deleted.is_empty() {
                    drained = true;
                    break;
                }
            }
            if !drained {
                skipped.push(mbox.clone());
            }

            total_found += mailbox_found;
            total_deleted += mailbox_deleted;
            total_failed += mailbox_failed;
            if mailbox_found > 0 {
                per_mailbox.push(PerMailboxDeleteResult {
                    mailbox: mbox.clone(),
                    found: mailbox_found,
                    deleted: mailbox_deleted,
                    failed: mailbox_failed,
                });
            }
        }

        session.release().await;

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
        expected_uid_validity: u32,
        destination: &str,
    ) -> Result<MoveMessageResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
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
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;
        self.fence_header_cache_mutation(account).await;
        imap_client::move_message(session.session(), uid, destination, &caps).await?;
        imap_client::sync(session.session()).await?;
        session.release().await;

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

        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);
        // Best-effort: create the drafts mailbox if it doesn't exist yet.
        // Many servers auto-create it, but some (Dovecot, etc.) require explicit CREATE first.
        // Ignore "already exists" errors; the APPEND below will surface real problems.
        let _created_drafts_mailbox = imap_client::create_mailbox(session.session(), &drafts_name)
            .await
            .is_ok();
        self.invalidate_mailbox_catalog(account);

        let append_result =
            imap_client::append_draft(session.session(), &drafts_name, &rfc822).await;
        self.invalidate_mailbox_catalog(account);
        append_result?;
        imap_client::sync(session.session()).await?;

        // Best-effort identity recovery: async-imap does not expose UIDPLUS
        // APPENDUID, so search the drafts mailbox for the Message-ID that
        // compose_draft generated. A recovery failure leaves the identity
        // fields unset without failing the successful create.
        let identity = match draft::extract_message_id(&rfc822) {
            Some(message_id) => {
                imap_client::find_uid_by_message_id(session.session(), &drafts_name, &message_id)
                    .await
                    .ok()
                    .flatten()
            }
            None => None,
        };
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
            uid_validity: identity.map(|(uid_validity, _)| uid_validity),
            uid: identity.map(|(_, uid)| uid),
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
        expected_uid_validity: u32,
    ) -> Result<GetMessageSourceResponse> {
        self.get_message_source_with_limit(
            mailbox,
            account,
            uid,
            expected_uid_validity,
            imap_client::MAX_TRANSIENT_MESSAGE_BYTES as u32,
        )
        .await
    }

    /// Get raw RFC822 source after a live UIDVALIDITY check and size preflight.
    pub async fn get_message_source_with_limit(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        max_bytes: u32,
    ) -> Result<GetMessageSourceResponse> {
        let raw = self
            .get_message_source_bytes_with_limit(
                mailbox,
                account,
                uid,
                expected_uid_validity,
                max_bytes,
            )
            .await?;

        let source = String::from_utf8_lossy(&raw).to_string();
        Ok(GetMessageSourceResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            source,
        })
    }

    /// Get exact raw RFC822 bytes after a live UIDVALIDITY check and size
    /// preflight. This is the lossless path used by the MCP source resource;
    /// callers that need the legacy string response can use
    /// [`Self::get_message_source_with_limit`].
    pub async fn get_message_source_bytes_with_limit(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        max_bytes: u32,
    ) -> Result<Vec<u8>> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        let mut session = self.pool.acquire(account).await?;
        let raw = imap_client::get_message_source_bounded(
            session.session(),
            mailbox,
            uid,
            expected_uid_validity,
            max_bytes as usize,
        )
        .await?;
        session.release().await;
        Ok(raw)
    }

    /// Get the exact RFC822 header block after validating the message epoch.
    pub async fn get_message_headers(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        max_bytes: u32,
    ) -> Result<String> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        let mut session = self.pool.acquire(account).await?;
        let headers = imap_client::get_message_headers_bounded(
            session.session(),
            mailbox,
            uid,
            expected_uid_validity,
            max_bytes as usize,
        )
        .await?;
        session.release().await;
        Ok(String::from_utf8_lossy(&headers).into_owned())
    }

    // -----------------------------------------------------------------
    // Download attachments
    // -----------------------------------------------------------------

    /// Fetch a message's raw source and parse out its MIME attachment parts
    /// as `(filename, content type, bytes)` triples in part order. Nameless
    /// parts get the filename "unnamed". UIDVALIDITY-guarded like every other
    /// delayed UID consumer.
    pub async fn get_attachment_data(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
    ) -> Result<Vec<(String, String, Vec<u8>)>> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        let mut session = self.pool.acquire(account).await?;
        let raw =
            imap_client::get_message_source(session.session(), mailbox, uid, expected_uid_validity)
                .await?;
        session.release().await;

        // Parse attachments on a blocking thread (CPU-intensive MIME parsing)
        tokio::task::spawn_blocking(move || parser::extract_attachment_data(&raw, uid))
            .await
            .map_err(|e| AgentmailError::Other(format!("spawn_blocking join error: {}", e)))?
    }

    /// Download attachments from a message to a directory.
    /// Files are named `{uid}_{index}_{original_name}`.
    pub async fn download_attachments(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        output_dir: &std::path::Path,
    ) -> Result<DownloadAttachmentsResponse> {
        let attachments = self
            .get_attachment_data(mailbox, account, uid, expected_uid_validity)
            .await?;

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

    /// Perform a UIDVALIDITY-guarded, DKIM-verified RFC 8058 one-click
    /// unsubscribe and optionally delete the same normalized List-Id across
    /// account storage mailboxes.
    ///
    /// Consent, cleanup-after-failure, sender fallback, and irreversible Trash
    /// fallback are independent explicit policies in `options`.
    pub async fn unsubscribe_message(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        options: UnsubscribeOptions,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<UnsubscribeResponse> {
        validate_unsubscribe_options(options)?;
        if uid == 0 {
            return Err(AgentmailError::MessageNotFound(0));
        }
        imap_client::check_cancel(cancel)?;

        // Bind the numeric UID to the exact epoch returned by discovery, then
        // fetch the complete message transiently for local DKIM verification.
        let mut session = self.pool.acquire(account).await?;
        let target = imap_client::fetch_unsubscribe_target(
            session.session(),
            mailbox,
            uid,
            options.expected_uid_validity,
            cancel,
        )
        .await?;
        let headers = unsubscribe::parse_list_headers(&target.raw_message);
        let (target_email, target_name, _, _) =
            parser::parse_sender_date(&target.raw_message).unwrap_or_default();
        session.release().await;

        let mut response = UnsubscribeResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            uid_validity: target.uid_validity,
            list_unsubscribe: headers.list_unsubscribe.clone(),
            list_unsubscribe_post: headers.list_unsubscribe_post.clone(),
            list_id: headers.list_id.clone(),
            pathway: headers
                .list_unsubscribe
                .as_ref()
                .map(|_| "rfc8058-one-click".to_string()),
            dkim_verified: false,
            list_id_authenticated: false,
            dkim_domain: None,
            unsubscribed: UnsubscribeResult {
                success: false,
                method: None,
                url: None,
                http_status: None,
                reason: None,
            },
            matching_messages: None,
            cleanup_skipped_reason: None,
        };

        let attempt = unsubscribe::attempt_one_click(&target.raw_message, &headers, cancel).await?;
        // DKIM is complete; do not retain a potentially large message while
        // scanning a large account for optional cleanup.
        drop(target.raw_message);
        response.list_id_authenticated = attempt.list_id_authenticated;
        response.dkim_domain = attempt.dkim_domain;
        response.dkim_verified = response.dkim_domain.is_some();
        response.unsubscribed = attempt.result;

        if !options.delete_matching {
            return Ok(response);
        }
        if !cleanup_policy_allows(options, response.unsubscribed.success) {
            response.cleanup_skipped_reason = Some(
                "Matching-message cleanup was skipped because the unsubscribe attempt failed and deleteOnUnsubscribeFailure was not explicitly enabled."
                    .to_string(),
            );
            return Ok(response);
        }

        let identity = match select_unsubscribe_cleanup_identity(
            headers.list_id.as_deref(),
            headers.has_single_list_id(),
            response.list_id_authenticated,
            &target_email,
            options.allow_sender_fallback,
        ) {
            Ok(identity) => identity,
            Err(CleanupIdentityError::UnauthenticatedListId) => {
                response.cleanup_skipped_reason = Some(
                    "Matching-message cleanup requires the single List-Id to be covered by the same passing DKIM signature as the RFC 8058 headers. Exact-sender fallback was not explicitly enabled."
                        .to_string(),
                );
                return Ok(response);
            }
            Err(CleanupIdentityError::NoUsableListId) => {
                response.cleanup_skipped_reason = Some(
                    "Matching-message cleanup requires one valid List-Id. Exact-sender fallback was not explicitly enabled."
                        .to_string(),
                );
                return Ok(response);
            }
        };

        let sender_display = if target_name.is_empty() {
            target_email.clone()
        } else {
            format!("{} <{}>", target_name, target_email)
        };

        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self
            .trash_for_mode(options.mode, account, session.session(), &caps)
            .await;
        if caps.is_gmail() && trash.is_none() {
            session.release().await;
            response.cleanup_skipped_reason = Some(
                "Matching-message cleanup was skipped because Gmail Trash could not be resolved; in-place EXPUNGE only removes a label and is not a permanent delete."
                    .to_string(),
            );
            return Ok(response);
        }
        if options.mode == DeleteMode::TrashFirst
            && trash.is_none()
            && !options.allow_permanent_fallback
        {
            session.release().await;
            response.cleanup_skipped_reason = Some(
                "Matching-message cleanup was skipped because no Trash mailbox was available and permanent fallback was not explicitly enabled."
                    .to_string(),
            );
            return Ok(response);
        }

        let all_mailboxes = self
            .account_scan_mailboxes(account, session.session(), scan_plan::ScanPurpose::Mutation)
            .await?;
        let mut total_found = 0usize;
        let mut total_deleted = 0usize;
        let mut total_failed = 0usize;
        let mut per_mailbox = Vec::new();
        let mut skipped = Vec::new();
        let mut cache_dirtied = false;
        let mut trash_fallback = options.mode == DeleteMode::TrashFirst && trash.is_none();

        for mbox in &all_mailboxes {
            let mut mailbox_found = 0usize;
            let mut mailbox_deleted = 0usize;
            let mut mailbox_failed = 0usize;
            let mut drained = false;
            for _pass in 0..Self::MAX_WINDOW_DRAIN_PASSES {
                imap_client::check_cancel(cancel)?;
                // Re-select each pass so a windowed server's freshly
                // backfilled messages become visible.
                let mb = match imap_client::select(session.session(), mbox).await {
                    Ok(mb) => mb,
                    Err(_) => {
                        skipped.push(mbox.clone());
                        drained = true;
                        break;
                    }
                };

                let exact_uids = match &identity {
                    CleanupIdentity::ListId { normalized, .. } => {
                        match self
                            .exact_list_id_uids(
                                session.session(),
                                account,
                                normalized,
                                mb.exists,
                                mb.uid_next,
                                cancel,
                            )
                            .await
                        {
                            Ok(uids) => uids,
                            Err(_) => {
                                skipped.push(mbox.clone());
                                drained = true;
                                break;
                            }
                        }
                    }
                    CleanupIdentity::Sender => {
                        let criteria = SearchCriteria {
                            from: Some(target_email.clone()),
                            deleted: Some(false),
                            ..Default::default()
                        };
                        let query = imap_client::build_search_query_pub(&criteria)?;
                        let candidates =
                            match imap_client::search_uids(session.session(), &query).await {
                                Ok(uids) => uids,
                                Err(_) => {
                                    skipped.push(mbox.clone());
                                    drained = true;
                                    break;
                                }
                            };
                        filter_sender_bulk_mail(
                            session.session(),
                            &candidates,
                            &target_email,
                            &target_name,
                            cancel,
                        )
                        .await?
                    }
                };

                if exact_uids.is_empty() {
                    drained = true;
                    break;
                }
                if !cache_dirtied {
                    self.fence_header_cache_mutation(account).await;
                    cache_dirtied = true;
                }

                let result = imap_client::bulk_delete_messages_with_policy(
                    session.session(),
                    &exact_uids,
                    trash.as_deref(),
                    &caps,
                    options.allow_permanent_fallback,
                    on_progress,
                    cancel,
                )
                .await?;
                imap_client::sync(session.session()).await?;
                trash_fallback |= result.trash_fallback;
                mailbox_found += exact_uids.len();
                mailbox_deleted += result.deleted.len();
                mailbox_failed += result.failed.len();
                if result.deleted.is_empty() {
                    drained = true;
                    break;
                }
            }
            if !drained {
                skipped.push(mbox.clone());
            }

            total_found += mailbox_found;
            total_deleted += mailbox_deleted;
            total_failed += mailbox_failed;
            if mailbox_found > 0 {
                per_mailbox.push(PerMailboxDeleteResult {
                    mailbox: mbox.clone(),
                    found: mailbox_found,
                    deleted: mailbox_deleted,
                    failed: mailbox_failed,
                });
            }
        }

        let (matched_by, list_id) = match identity {
            CleanupIdentity::ListId { raw, .. } => ("list-id", Some(raw)),
            CleanupIdentity::Sender => ("exact-sender-fallback", None),
        };
        let complete = skipped.is_empty() && total_failed == 0;
        response.matching_messages = Some(MatchingMessagesResult {
            matched_by: matched_by.to_string(),
            sender: sender_display,
            list_id,
            found: total_found,
            deleted: total_deleted,
            failed: total_failed,
            mailboxes: per_mailbox,
            skipped,
            // Gmail's safe provider-specific interpretation of Permanent is
            // a move to Trash: in-place UID EXPUNGE only removes a label.
            permanent: (options.mode == DeleteMode::Permanent && !caps.is_gmail())
                || trash_fallback,
            trash_fallback,
            complete,
        });
        session.release().await;
        Ok(response)
    }

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

    /// Build an account-wide scan plan from the bounded mailbox-layout
    /// catalog. The catalog contains no message state.
    async fn account_scan_mailboxes(
        &self,
        account: &str,
        session: &mut imap_client::ImapSession,
        purpose: scan_plan::ScanPurpose,
    ) -> Result<Vec<String>> {
        let entries = self
            .mailbox_catalog
            .get_or_refresh(account, || imap_client::list_mailbox_layout(session))
            .await?;
        let plan = scan_plan::plan_account_scan(&entries, purpose);
        tracing::debug!(
            target: "agentmail",
            operation = "account_scan_plan",
            purpose = purpose.as_str(),
            strategy = plan.strategy.as_str(),
            catalog_count = entries.len(),
            result_count = plan.mailboxes.len(),
            "account-wide mailbox scan planned"
        );
        Ok(plan.mailboxes)
    }

    /// Resolve this account's Trash and Drafts mailbox names from the bounded
    /// mailbox-layout catalog. One cold `LIST` resolves both roles.
    async fn special_mailboxes(
        &self,
        account: &str,
        session: &mut imap_client::ImapSession,
    ) -> Result<(Option<String>, Option<String>)> {
        let entries = self
            .mailbox_catalog
            .get_or_refresh(account, || imap_client::list_mailbox_layout(session))
            .await?;
        let trash = mailbox_catalog::resolve_trash(&entries);
        let drafts = mailbox_catalog::resolve_drafts(&entries);
        Ok((trash, drafts))
    }

    /// Resolve the trash destination for a delete, honoring the delete mode.
    /// `Permanent` bypasses Trash (straight to flag + UID EXPUNGE) — except on
    /// Gmail, where in-place EXPUNGE only removes a label, so even a permanent
    /// delete must move to `[Gmail]/Trash` (Gmail purges Trash on its own).
    async fn trash_for_mode(
        &self,
        mode: DeleteMode,
        account: &str,
        session: &mut imap_client::ImapSession,
        caps: &imap_client::ServerCaps,
    ) -> Option<String> {
        if matches!(mode, DeleteMode::Permanent) && !caps.is_gmail() {
            return None;
        }
        self.special_mailboxes(account, session)
            .await
            .ok()
            .and_then(|(trash, _)| trash)
    }

    /// Invalidate cached layout after a mailbox mutation.
    fn invalidate_mailbox_catalog(&self, account: &str) {
        self.mailbox_catalog.invalidate(account);
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum CleanupIdentity {
    ListId { raw: String, normalized: String },
    Sender,
}

#[derive(Debug, PartialEq, Eq)]
enum CleanupIdentityError {
    UnauthenticatedListId,
    NoUsableListId,
}

fn select_unsubscribe_cleanup_identity(
    list_id: Option<&str>,
    has_single_list_id: bool,
    list_id_authenticated: bool,
    target_email: &str,
    allow_sender_fallback: bool,
) -> std::result::Result<CleanupIdentity, CleanupIdentityError> {
    let normalized = has_single_list_id
        .then_some(list_id)
        .flatten()
        .and_then(normalize_list_id);

    match normalized {
        Some(normalized) if list_id_authenticated => Ok(CleanupIdentity::ListId {
            raw: list_id.unwrap_or_default().to_string(),
            normalized,
        }),
        _ if allow_sender_fallback && !target_email.is_empty() => Ok(CleanupIdentity::Sender),
        Some(_) => Err(CleanupIdentityError::UnauthenticatedListId),
        None => Err(CleanupIdentityError::NoUsableListId),
    }
}

fn validate_unsubscribe_options(options: UnsubscribeOptions) -> Result<()> {
    if options.expected_uid_validity == 0 {
        return Err(AgentmailError::InvalidUnsubscribePolicy(
            "expected_uid_validity must be a non-zero value returned by top_subscriptions"
                .to_string(),
        ));
    }
    if !options.confirm_one_click {
        return Err(AgentmailError::UnsubscribeConsentRequired);
    }
    if options.delete_on_unsubscribe_failure && !options.delete_matching {
        return Err(AgentmailError::InvalidUnsubscribePolicy(
            "delete_on_unsubscribe_failure requires delete_matching=true".to_string(),
        ));
    }
    if options.allow_sender_fallback && !options.delete_matching {
        return Err(AgentmailError::InvalidUnsubscribePolicy(
            "allow_sender_fallback requires delete_matching=true".to_string(),
        ));
    }
    if options.allow_permanent_fallback && !options.delete_matching {
        return Err(AgentmailError::InvalidUnsubscribePolicy(
            "allow_permanent_fallback requires delete_matching=true".to_string(),
        ));
    }
    if options.mode == DeleteMode::Permanent && options.allow_permanent_fallback {
        return Err(AgentmailError::InvalidUnsubscribePolicy(
            "allow_permanent_fallback is redundant when permanent=true".to_string(),
        ));
    }
    Ok(())
}

fn cleanup_policy_allows(options: UnsubscribeOptions, unsubscribe_succeeded: bool) -> bool {
    options.delete_matching && (unsubscribe_succeeded || options.delete_on_unsubscribe_failure)
}

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

/// Pick a stable representative when dates are absent or tied. IMAP UIDs are
/// mailbox-local, so mailbox name is the primary tie-breaker and UID the
/// secondary one.
fn ranking_sample_is_newer(
    candidate: (Option<chrono::DateTime<chrono::Utc>>, &str, u32),
    current: (Option<chrono::DateTime<chrono::Utc>>, Option<&str>, u32),
    first: bool,
) -> bool {
    if first {
        return true;
    }

    let (candidate_date, candidate_mailbox, candidate_uid) = candidate;
    let (current_date, current_mailbox, current_uid) = current;
    match (candidate_date, current_date) {
        (Some(candidate), Some(current)) if candidate != current => candidate > current,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => {
            (candidate_mailbox, candidate_uid) > (current_mailbox.unwrap_or_default(), current_uid)
        }
    }
}

pub(crate) fn next_offset(offset: usize, item_count: usize, total: usize) -> Option<usize> {
    let next = offset.saturating_add(item_count);
    (item_count > 0 && next < total).then_some(next)
}

/// Whether a message's `List-Id` header value matches the requested List-Id.
/// IMAP `HEADER` search is substring-only, so `delete_list_id` confirms the
/// exact list here before deleting. Compared case-insensitively after trimming;
/// the value round-trips exactly from `top_mailing_lists`'s `listId` output.
fn list_id_matches(requested: &str, candidate: &str) -> bool {
    match (normalize_list_id(requested), normalize_list_id(candidate)) {
        (Some(requested), Some(candidate)) => requested == candidate,
        _ => requested.trim().eq_ignore_ascii_case(candidate.trim()),
    }
}

/// Return the RFC 2919 list identifier inside angle brackets, normalized for
/// exact comparison. The optional display phrase is deliberately ignored so
/// harmless name changes cannot split one list or broaden a deletion.
fn normalize_list_id(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(start) = value.find('<') {
        let end = value[start + 1..].find('>')? + start + 1;
        if !value[end + 1..].trim().is_empty()
            || value[..start].contains(['<', '>'])
            || value[start + 1..end].contains(['<', '>', ' ', '\t', '\r', '\n'])
        {
            return None;
        }
        let identifier = value[start + 1..end].trim();
        return (!identifier.is_empty()).then(|| identifier.to_ascii_lowercase());
    }

    (!value.is_empty() && !value.contains(['<', '>', ' ', '\t', '\r', '\n']))
        .then(|| value.to_ascii_lowercase())
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
pub(crate) fn sanitize_filename(name: &str) -> String {
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

    #[test]
    fn own_addresses_returns_lowercased_username() {
        let cfg = Config::from_accounts(vec![(
            "work".to_string(),
            config::AccountConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                username: "Me@Example.COM".to_string(),
                password: None,
                tls: true,
                max_connections: None,
            },
        )]);
        let mk = Agentmail::new(cfg);
        let own = mk.own_addresses("work");
        assert!(own.contains("me@example.com"));
        assert!(!own.contains("someone@else.com"));
        // Unknown account → empty set (nothing excluded).
        assert!(mk.own_addresses("missing").is_empty());
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
        assert!(list_id_matches(
            "Old Name <news.example.com>",
            "New Name <NEWS.EXAMPLE.COM>"
        ));
        assert!(!list_id_matches(
            "News <news.example.com>",
            "News <other.example.com>"
        ));
    }

    fn unsubscribe_options() -> UnsubscribeOptions {
        UnsubscribeOptions {
            expected_uid_validity: 7,
            confirm_one_click: true,
            delete_matching: false,
            delete_on_unsubscribe_failure: false,
            allow_sender_fallback: false,
            allow_permanent_fallback: false,
            mode: DeleteMode::TrashFirst,
        }
    }

    #[test]
    fn unsubscribe_requires_explicit_consent_and_nonzero_uidvalidity() {
        let mut options = unsubscribe_options();
        options.confirm_one_click = false;
        assert!(matches!(
            validate_unsubscribe_options(options),
            Err(AgentmailError::UnsubscribeConsentRequired)
        ));

        let mut options = unsubscribe_options();
        options.expected_uid_validity = 0;
        assert!(matches!(
            validate_unsubscribe_options(options),
            Err(AgentmailError::InvalidUnsubscribePolicy(_))
        ));
    }

    #[test]
    fn cleanup_failure_policy_is_explicit() {
        let mut options = unsubscribe_options();
        assert!(!cleanup_policy_allows(options, true));
        assert!(!cleanup_policy_allows(options, false));

        options.delete_matching = true;
        assert!(cleanup_policy_allows(options, true));
        assert!(!cleanup_policy_allows(options, false));

        options.delete_on_unsubscribe_failure = true;
        assert!(cleanup_policy_allows(options, false));
    }

    #[test]
    fn cleanup_identity_requires_dkim_authenticated_list_id() {
        let list_id = Some("Newsletter <news.example.com>");
        assert_eq!(
            select_unsubscribe_cleanup_identity(list_id, true, true, "sender@example.com", false),
            Ok(CleanupIdentity::ListId {
                raw: "Newsletter <news.example.com>".to_string(),
                normalized: "news.example.com".to_string(),
            })
        );
        assert_eq!(
            select_unsubscribe_cleanup_identity(list_id, true, false, "sender@example.com", false),
            Err(CleanupIdentityError::UnauthenticatedListId)
        );
        assert_eq!(
            select_unsubscribe_cleanup_identity(list_id, true, false, "sender@example.com", true),
            Ok(CleanupIdentity::Sender)
        );
        assert_eq!(
            select_unsubscribe_cleanup_identity(list_id, false, true, "sender@example.com", false),
            Err(CleanupIdentityError::NoUsableListId)
        );
        assert_eq!(
            select_unsubscribe_cleanup_identity(None, false, false, "", true),
            Err(CleanupIdentityError::NoUsableListId)
        );
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
