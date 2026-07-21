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

pub use config::{AccountConfig, AuthMethod, Config};
pub use connection::{
    ConnectionPool, ConnectionStats, is_login_rate_limited_host, recommended_max_connections,
};
pub use error::{AgentmailError, Result};
pub use imap_client::{CancelFn, ClientIdentity, ProgressFn};
pub use provider::MailProvider;
pub use secret::init_service_name;
pub use types::*;

use tokio::io::{AsyncRead, AsyncWrite};

/// High-level facade for IMAP operations.
/// Owns the connection pool and configuration.
pub struct Agentmail {
    pool: ConnectionPool,
    /// Per-account mailbox hierarchy used by completion and special-use lookup.
    mailbox_catalog: mailbox_catalog::MailboxCatalog,
    /// Persistent UID membership and immutable ranking-header projection.
    header_cache: header_cache::HeaderCache,
}

/// Where the header cache lives — the builder's programmatic answer to the
/// `AGENTMAIL_CACHE_DIR` / `AGENTMAIL_DISABLE_HEADER_CACHE` environment
/// variables. An explicit choice overrides both variables.
#[derive(Debug, Clone, Default)]
enum CacheLocation {
    /// Environment-aware default (env override, else the OS cache dir).
    #[default]
    Auto,
    /// No persistence: rankings use the Limited-Mode live fallback and UID
    /// Mode is never entered.
    Disabled,
    /// Persist under this directory (the versioned cache file is created
    /// inside it).
    Dir(std::path::PathBuf),
}

/// Programmatic configuration for embedding [`Agentmail`] in an application —
/// every knob here works without environment variables, and explicit choices
/// override the corresponding variables.
///
/// ```no_run
/// # use agentmail::{Agentmail, Config};
/// # use std::time::Duration;
/// # let config = Config::empty();
/// let mail = Agentmail::builder(config)
///     .cache_dir("/path/to/app/caches")
///     .imap_timeout(Duration::from_secs(120))
///     .login_cooldown(Duration::from_secs(600))
///     .build();
/// ```
pub struct AgentmailBuilder {
    config: Config,
    cache: CacheLocation,
    login_cooldown: Option<std::time::Duration>,
    imap_timeout: Option<std::time::Duration>,
    max_idle: Option<std::time::Duration>,
    keepalive: Option<std::time::Duration>,
    client_identity: Option<ClientIdentity>,
}

impl AgentmailBuilder {
    /// Persist the header cache inside this directory (created on first use).
    /// Overrides `AGENTMAIL_CACHE_DIR` and `AGENTMAIL_DISABLE_HEADER_CACHE`.
    /// The cache powers UID-Mode full-mailbox ranking; on windowed providers
    /// (Yahoo/AOL) disabling it limits rankings to the visible window.
    pub fn cache_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.cache = CacheLocation::Dir(dir.into());
        self
    }

    /// Disable header-cache persistence entirely (see [`Self::cache_dir`] for
    /// the ranking consequences). Overrides both cache environment variables.
    pub fn disable_cache(mut self) -> Self {
        self.cache = CacheLocation::Disabled;
        self
    }

    /// How long to refuse new logins for an account after its server answers
    /// a LOGIN rate limit (default 300s). Floored at 1s.
    pub fn login_cooldown(mut self, cooldown: std::time::Duration) -> Self {
        self.login_cooldown = Some(cooldown);
        self
    }

    /// Per-command IMAP operation timeout (default 90s; floored at 1s).
    /// Process-wide: the timeout wraps every command on every session.
    pub fn imap_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.imap_timeout = Some(timeout);
        self
    }

    /// How long an idle pooled session stays eligible for reuse before the
    /// next acquire reconnects fresh instead (default 5 min; floored at 1s).
    /// Sessions are liveness-pinged before reuse regardless, so raising this
    /// only risks a failed ping — worth it on login-rate-limited providers.
    pub fn max_idle(mut self, max_idle: std::time::Duration) -> Self {
        self.max_idle = Some(max_idle);
        self
    }

    /// Keep every idle pooled session — Limited pool and UID-Mode pool alike —
    /// alive with a background NOOP on this interval (floored at 30s). The
    /// process then behaves like a mainstream mail client: a few long-lived
    /// connections, each LOGINed exactly once, instead of a fresh login per
    /// gap in traffic. (IMAP requires one LOGIN per TCP connection; keeping
    /// connections alive is the only way to "log in once".) The task starts
    /// on first pool use and stops when this `Agentmail` is dropped. Any
    /// interval shorter than `max_idle` works — the keepalive refreshes the
    /// idle stamp on every successful ping.
    pub fn keepalive(mut self, interval: std::time::Duration) -> Self {
        self.keepalive = Some(interval);
        self
    }

    /// The RFC 2971 `ID` identity sent to servers at connect — the embedding
    /// application's name/version (and optionally vendor, support URL, OS
    /// version), replacing the library default. Yahoo/AOL ask clients to
    /// identify themselves and key partner registration on `name`; RFC 2971
    /// requires the values be truthful. Process-wide.
    pub fn client_identity(mut self, identity: ClientIdentity) -> Self {
        self.client_identity = Some(identity);
        self
    }

    pub fn build(self) -> Agentmail {
        if let Some(timeout) = self.imap_timeout {
            imap_client::set_imap_timeout(timeout);
        }
        if let Some(identity) = self.client_identity {
            imap_client::set_client_identity(identity);
        }
        let mut pool = ConnectionPool::new(self.config);
        if let Some(cooldown) = self.login_cooldown {
            pool.set_login_cooldown(cooldown);
        }
        if let Some(max_idle) = self.max_idle {
            pool.set_max_idle(max_idle);
        }
        if let Some(interval) = self.keepalive {
            pool.set_keepalive(interval);
        }
        let header_cache = match self.cache {
            CacheLocation::Auto => header_cache::HeaderCache::default(),
            CacheLocation::Disabled => header_cache::HeaderCache::disabled(),
            CacheLocation::Dir(dir) => {
                header_cache::HeaderCache::at_path(dir.join(header_cache::HeaderCache::FILE_NAME))
            }
        };
        Agentmail {
            pool,
            mailbox_catalog: mailbox_catalog::MailboxCatalog::default(),
            header_cache,
        }
    }
}

impl Agentmail {
    /// Create from an existing config with environment-aware defaults.
    /// Embedding applications that need explicit settings use
    /// [`Agentmail::builder`] instead.
    pub fn new(config: Config) -> Self {
        Self::builder(config).build()
    }

    /// Start building an [`Agentmail`] with programmatic settings — cache
    /// location, login-rate-limit cooldown, and IMAP timeout — none of which
    /// require environment variables.
    pub fn builder(config: Config) -> AgentmailBuilder {
        AgentmailBuilder {
            config,
            cache: CacheLocation::default(),
            login_cooldown: None,
            imap_timeout: None,
            max_idle: None,
            keepalive: None,
            client_identity: None,
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

    /// Snapshot the connection-lifecycle counters — the evidence for whether
    /// connections are being held and reused rather than re-LOGINed per call.
    /// See [`connection::ConnectionStats`].
    pub fn connection_stats(&self) -> connection::ConnectionStats {
        self.pool.connection_stats()
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
    /// Attempts for a ranking scan whose session died mid-flight (throttling
    /// servers issue BYE during long cold scans). Progress is chunk-committed
    /// to the header cache, so a resumed attempt reuses everything already
    /// fetched instead of starting over.
    const SCAN_RESUME_ATTEMPTS: usize = 3;

    /// Enter RFC 9586 UID Mode when the server supports it, so a ranking scan
    /// sees the WHOLE mailbox instead of the newest-N visible window (Yahoo/AOL
    /// `UIDONLY`). Returns the per-command page size (`MESSAGELIMIT`) to walk
    /// by, or `None` for Limited Mode.
    ///
    /// A UID-Mode session changes every response to `UIDFETCH` for the rest of
    /// its life, so callers MUST mark it (`mark_uid_mode`) so release routes it
    /// to the UID-Mode store, never the Limited pool.
    async fn enter_uid_mode(
        &self,
        account: &str,
        session: &mut imap_client::ImapSession,
    ) -> Result<Option<u32>> {
        // UID Mode's whole-mailbox walk only pays off through the cache; with
        // the cache disabled, the Limited-Mode live fallback (windowed) is the
        // only working path, so stay in Limited Mode.
        if !self.header_cache.is_persistent() {
            return Ok(None);
        }
        let caps = self.pool.server_caps(account, session).await?;
        if !caps.has("UIDONLY") {
            return Ok(None);
        }
        let page = caps
            .message_limit()
            .unwrap_or(imap_client::MAX_FETCH_CHUNK as u32);
        imap_client::enable(session, "UIDONLY").await?;
        Ok(Some(page))
    }

    /// Acquire a session for a UID-Mode-capable scan, preferring an idle
    /// pooled UID-Mode session — `ENABLE UIDONLY` is sticky for a connection's
    /// life, so reuse skips a whole LOGIN + ENABLE. On rate-limited providers
    /// (AOL/Yahoo `[LIMIT] LOGIN`) this is the difference between one login
    /// per process and one per tool call. Falls back to a normal acquire plus
    /// [`Self::enter_uid_mode`].
    async fn acquire_uid_scan(
        &self,
        account: &str,
    ) -> Result<(connection::PooledSession, Option<u32>)> {
        if self.header_cache.is_persistent()
            && let Some(caps) = self.pool.cached_caps(account)
            && caps.has("UIDONLY")
            && let Some(session) = self.pool.try_acquire_uid_mode(account).await?
        {
            let page = caps
                .message_limit()
                .unwrap_or(imap_client::MAX_FETCH_CHUNK as u32);
            return Ok((session, Some(page)));
        }
        let mut session = self.pool.acquire(account).await?;
        let uid_mode = self.enter_uid_mode(account, session.session()).await?;
        if uid_mode.is_some() {
            session.mark_uid_mode();
        }
        Ok((session, uid_mode))
    }

    /// Fetch page-sample subjects and prune the samples the server no longer
    /// has (fetch succeeded, row absent — the deleted-message signal). The
    /// pruned rows drop out of the projection AND membership together, so the
    /// very next ranking call auto-advances each group to its next-newest
    /// sample instead of re-serving a dead UID.
    async fn page_sample_subjects(
        &self,
        session: &mut imap_client::ImapSession,
        account: &str,
        samples: &[MailboxMessageIdentity],
        cancel: Option<&CancelFn>,
    ) -> hashbrown::HashMap<(String, u32), String> {
        let SampleSubjects { subjects, missing } = sample_subjects(session, samples, cancel).await;
        if !missing.is_empty() {
            tracing::debug!(
                target: "agentmail",
                pruned = missing.len(),
                "pruning ranking samples the server no longer has"
            );
            if let Some(config) = self.pool.account_config(account) {
                for (mailbox, uid) in &missing {
                    self.header_cache
                        .prune_uid(account, config, mailbox, *uid)
                        .await;
                }
            }
        }
        subjects
    }

    /// Release a scan session: `PooledSession::release` routes a marked
    /// UID-Mode session to the UID-Mode store and a Limited-Mode one to the
    /// shared pool. Kept as a named helper so scan call sites read explicitly.
    async fn uid_mode_release(session: connection::PooledSession, _uid_mode: Option<u32>) {
        session.release().await;
    }

    async fn scan_resume_backoff(
        attempt: usize,
        scan: &str,
        cancel: Option<&CancelFn>,
    ) -> Result<()> {
        imap_client::check_cancel(cancel)?;
        tracing::warn!(
            target: "agentmail",
            scan,
            attempt,
            "connection dropped during ranking scan; resuming with a fresh session (progress is cached)",
        );
        tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
        imap_client::check_cancel(cancel)?;
        Ok(())
    }

    pub async fn top_senders(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopSendersResponse> {
        for attempt in 1..=Self::SCAN_RESUME_ATTEMPTS {
            match self
                .top_senders_once(mailbox, account, offset, limit, on_progress, cancel)
                .await
            {
                Err(error)
                    if error.is_connection_error() && attempt < Self::SCAN_RESUME_ATTEMPTS =>
                {
                    Self::scan_resume_backoff(attempt, "top_senders", cancel).await?;
                }
                other => return other,
            }
        }
        unreachable!("the final attempt always returns")
    }

    async fn top_senders_once(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopSendersResponse> {
        let (mut session, uid_mode) = self.acquire_uid_scan(account).await?;

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
                uid_mode,
                &own_vec,
                offset,
                limit,
                on_progress,
                cancel,
            )
            .await?
        {
            Self::uid_mode_release(session, uid_mode).await;
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
        for attempt in 1..=Self::SCAN_RESUME_ATTEMPTS {
            match self
                .top_subscriptions_once(mailbox, account, offset, limit, on_progress, cancel)
                .await
            {
                Err(error)
                    if error.is_connection_error() && attempt < Self::SCAN_RESUME_ATTEMPTS =>
                {
                    Self::scan_resume_backoff(attempt, "top_subscriptions", cancel).await?;
                }
                other => return other,
            }
        }
        unreachable!("the final attempt always returns")
    }

    async fn top_subscriptions_once(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopSubscriptionsResponse> {
        let (mut session, uid_mode) = self.acquire_uid_scan(account).await?;

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
                uid_mode,
                &own_vec,
                offset,
                limit,
                on_progress,
                cancel,
            )
            .await?
        {
            let item_count = page.items.len();
            let mut lists: Vec<ListSummary> = page
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
                    subject: None,
                    count: u32::try_from(row.count).unwrap_or(u32::MAX),
                    oldest_date: row.oldest_date,
                    newest_date: row.newest_date,
                })
                .collect();
            let samples: Vec<MailboxMessageIdentity> =
                lists.iter().map(|row| row.sample.clone()).collect();
            let subjects = self
                .page_sample_subjects(session.session(), account, &samples, cancel)
                .await;
            for row in &mut lists {
                row.subject = subjects
                    .get(&(row.sample.mailbox.clone(), row.sample.uid))
                    .cloned();
            }
            Self::uid_mode_release(session, uid_mode).await;
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
                    subject: None,
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
        let mut lists: Vec<_> = lists.into_iter().skip(offset).take(limit).collect();
        let item_count = lists.len();

        // Subjects only for the page actually returned, on the session still
        // held; released after so the enrichment reuses the same connection.
        let samples: Vec<MailboxMessageIdentity> =
            lists.iter().map(|row| row.sample.clone()).collect();
        let subjects = self
            .page_sample_subjects(session.session(), account, &samples, cancel)
            .await;
        for row in &mut lists {
            row.subject = subjects
                .get(&(row.sample.mailbox.clone(), row.sample.uid))
                .cloned();
        }
        session.release().await;

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
        for attempt in 1..=Self::SCAN_RESUME_ATTEMPTS {
            match self
                .top_mailing_lists_once(mailbox, account, offset, limit, on_progress, cancel)
                .await
            {
                Err(error)
                    if error.is_connection_error() && attempt < Self::SCAN_RESUME_ATTEMPTS =>
                {
                    Self::scan_resume_backoff(attempt, "top_mailing_lists", cancel).await?;
                }
                other => return other,
            }
        }
        unreachable!("the final attempt always returns")
    }

    async fn top_mailing_lists_once(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopMailingListsResponse> {
        let (mut session, uid_mode) = self.acquire_uid_scan(account).await?;

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
                uid_mode,
                offset,
                limit,
                on_progress,
                cancel,
            )
            .await?
        {
            let item_count = page.items.len();
            let mut lists: Vec<ListIdSummary> = page
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
                    subject: None,
                    oldest_date: row.oldest_date,
                    newest_date: row.newest_date,
                })
                .collect();
            let samples: Vec<MailboxMessageIdentity> =
                lists.iter().map(|row| row.sample.clone()).collect();
            let subjects = self
                .page_sample_subjects(session.session(), account, &samples, cancel)
                .await;
            for row in &mut lists {
                row.subject = subjects
                    .get(&(row.sample.mailbox.clone(), row.sample.uid))
                    .cloned();
            }
            Self::uid_mode_release(session, uid_mode).await;
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
                    subject: None,
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
        let mut lists: Vec<_> = lists.into_iter().skip(offset).take(limit).collect();
        let item_count = lists.len();

        // Subjects only for the page actually returned, on the session still
        // held; released after so the enrichment reuses the same connection.
        let samples: Vec<MailboxMessageIdentity> =
            lists.iter().map(|row| row.sample.clone()).collect();
        let subjects = self
            .page_sample_subjects(session.session(), account, &samples, cancel)
            .await;
        for row in &mut lists {
            row.subject = subjects
                .get(&(row.sample.mailbox.clone(), row.sample.uid))
                .cloned();
        }
        session.release().await;

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
    async fn exact_list_id_uids<T>(
        &self,
        session: &mut async_imap::Session<T>,
        account: &str,
        mailbox: &str,
        list_id: &str,
        mailbox_exists: u32,
        mailbox_uid_next: Option<u32>,
        mailbox_uid_validity: Option<u32>,
        cancel: Option<&CancelFn>,
    ) -> Result<Vec<u32>>
    where
        T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
    {
        let criteria = SearchCriteria {
            header: Some(("List-Id".to_string(), list_id.to_string())),
            deleted: Some(false),
            ..Default::default()
        };
        let query = imap_client::build_search_query_pub(&criteria)?;
        // A rejected search and an empty one are indistinguishable on servers
        // whose tagged NO is swallowed by the client library; both fall
        // through to the trust check below.
        let mut candidates = imap_client::search_uids(session, &query)
            .await
            .unwrap_or_default();
        if candidates.is_empty() && mailbox_exists > 0 {
            // Two independent reasons to distrust an empty result: this
            // process observed the List-header quirk (fast, but unarmed after
            // a restart once the cache has healed), or the persisted
            // projection knows this mailbox holds matches the server search
            // failed to find (restart-proof ground truth).
            let projected_uids = match (
                normalize_list_id(list_id),
                self.pool.account_config(account),
                mailbox_uid_validity,
            ) {
                (Some(normalized), Some(config), Some(uid_validity)) => {
                    self.header_cache
                        .cached_list_id_uids(account, config, mailbox, &normalized, uid_validity)
                        .await
                }
                _ => Vec::new(),
            };
            if !projected_uids.is_empty() {
                // Fast path: confirm the projection's own UIDs — orders of
                // magnitude cheaper than enumerating a 100k-message window.
                // Fresh on the first drain pass; on later passes these UIDs
                // are already deleted, confirm to nothing, and fall through
                // to enumeration so window backfill is still covered.
                let confirmed =
                    confirm_exact_list_id(session, &projected_uids, list_id, cancel).await?;
                if !confirmed.is_empty() {
                    return Ok(confirmed);
                }
            }
            if !projected_uids.is_empty() || self.list_search_untrusted(account) {
                tracing::warn!(
                    target: "agentmail",
                    mailbox,
                    projected = projected_uids.len(),
                    "server List-Id search cannot be trusted here; enumerating the visible mailbox",
                );
                candidates =
                    imap_client::search_all_uids_checked(session, mailbox_exists, mailbox_uid_next)
                        .await?;
            }
        }
        confirm_exact_list_id(session, &candidates, list_id, cancel).await
    }

    /// Discover the UIDs to delete in `mbox` for one sweep pass. This is the
    /// only step that differs between the list-id, exact-sender, and
    /// unsubscribe-cleanup deletes — the surrounding drain loop is shared in
    /// [`Self::delete_sweep`]. `mb` is the freshly selected mailbox state.
    async fn discover_delete_uids<T>(
        &self,
        selector: &DeleteSelector,
        session: &mut async_imap::Session<T>,
        account: &str,
        mbox: &str,
        mb: &async_imap::types::Mailbox,
        cancel: Option<&CancelFn>,
    ) -> Result<Vec<u32>>
    where
        T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
    {
        match selector {
            DeleteSelector::ListId(list_id) => {
                // `exact_list_id_uids` normalizes internally, so the raw or
                // normalized value is equivalent here.
                self.exact_list_id_uids(
                    session,
                    account,
                    mbox,
                    list_id,
                    mb.exists,
                    mb.uid_next,
                    mb.uid_validity,
                    cancel,
                )
                .await
            }
            DeleteSelector::Sender {
                email,
                name,
                bulk_only,
                list_id,
            } => {
                let criteria = SearchCriteria {
                    from: Some(email.clone()),
                    deleted: Some(false),
                    ..Default::default()
                };
                let query = imap_client::build_search_query_pub(&criteria)?;
                let candidates = imap_client::search_uids(session, &query).await?;
                if candidates.is_empty() {
                    return Ok(Vec::new());
                }
                if *bulk_only {
                    // Unsubscribe cleanup: only the sender's bulk mail (must
                    // carry a List-Unsubscribe header), never personal replies.
                    filter_sender_bulk_mail(
                        session,
                        &candidates,
                        email,
                        name,
                        list_id.as_deref(),
                        cancel,
                    )
                    .await
                } else {
                    // delete_by_sender: every message from the exact identity.
                    let fetched =
                        imap_client::fetch_senders_batch(session, &candidates, cancel).await?;
                    Ok(fetched
                        .into_iter()
                        .filter(|(_uid, e, n)| e == email && n == name)
                        .map(|(uid, _, _)| uid)
                        .collect())
                }
            }
        }
    }

    /// UID-Mode-aware matching sweep shared by the delete flows
    /// (delete-by-list-id, delete-by-sender, unsubscribe cleanup) and the bulk
    /// move flows (move-by-list-id, move-by-sender) — `action` picks the fate
    /// of each discovered batch. Enters UID Mode when the server and cache
    /// allow, so a single pass discovers every match across the whole mailbox
    /// (crucial for sender selectors, which have no projection fast-path);
    /// otherwise it drains the visible window one backfilled page at a time.
    /// Owns `session` so it releases correctly by UID-Mode mark.
    async fn matching_sweep(
        &self,
        mut session: connection::PooledSession,
        account: &str,
        selector: DeleteSelector,
        mailboxes: &[String],
        action: SweepAction<'_>,
        caps: &imap_client::ServerCaps,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<SweepTotals> {
        // ENABLE UIDONLY may leave the session in an indeterminate state on
        // failure, so on error `session` falls out of scope here and the
        // connection closes rather than returning a half-configured session to
        // the pool.
        let uid_mode = self.enter_uid_mode(account, session.session()).await?;
        if uid_mode.is_some() {
            // Route release to the UID-Mode store so the next ranking scan or
            // sweep reuses this connection instead of paying another LOGIN.
            session.mark_uid_mode();
        }
        let outcome = self
            .matching_sweep_loop(
                session.session(),
                account,
                &selector,
                mailboxes,
                action,
                caps,
                on_progress,
                cancel,
            )
            .await;
        // Release routes by the UID-Mode mark: UID store or the Limited pool.
        Self::uid_mode_release(session, uid_mode).await;
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    async fn matching_sweep_loop<T>(
        &self,
        session: &mut async_imap::Session<T>,
        account: &str,
        selector: &DeleteSelector,
        mailboxes: &[String],
        action: SweepAction<'_>,
        caps: &imap_client::ServerCaps,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<SweepTotals>
    where
        T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
    {
        let mut totals = SweepTotals::default();
        let mut cache_dirtied = false;

        for mbox in mailboxes {
            let mut mailbox_found = 0usize;
            let mut mailbox_affected = 0usize;
            let mut mailbox_failed = 0usize;
            let mut drained = false;
            for _pass in 0..Self::MAX_WINDOW_DRAIN_PASSES {
                imap_client::check_cancel(cancel)?;
                // Re-select each pass so a windowed server's freshly backfilled
                // messages become visible (a no-op once in UID Mode, where the
                // whole mailbox is already visible).
                let mb = match imap_client::select(session, mbox).await {
                    Ok(mb) => mb,
                    Err(_) => {
                        totals.skipped.push(mbox.clone());
                        drained = true;
                        break;
                    }
                };
                let uids = match self
                    .discover_delete_uids(selector, session, account, mbox, &mb, cancel)
                    .await
                {
                    Ok(uids) => uids,
                    // A discovery failure marks the mailbox skipped (coverage
                    // incomplete) rather than aborting the account-wide sweep —
                    // but a pending cancellation must still propagate.
                    Err(_) => {
                        imap_client::check_cancel(cancel)?;
                        totals.skipped.push(mbox.clone());
                        drained = true;
                        break;
                    }
                };
                if uids.is_empty() {
                    drained = true;
                    break;
                }
                if !cache_dirtied {
                    // Both actions change mailbox membership, so the ranking
                    // projections must resync either way.
                    self.fence_header_cache_mutation(account).await;
                    cache_dirtied = true;
                }
                let (affected, failed) = match action {
                    SweepAction::Delete {
                        trash,
                        allow_permanent_fallback,
                    } => {
                        let result = imap_client::bulk_delete_messages_with_policy(
                            session,
                            &uids,
                            trash,
                            caps,
                            allow_permanent_fallback,
                            on_progress,
                            cancel,
                        )
                        .await?;
                        totals.trash_fallback |= result.trash_fallback;
                        (result.deleted.len(), result.failed.len())
                    }
                    SweepAction::Move { destination } => {
                        let result = imap_client::bulk_move_messages(
                            session,
                            &uids,
                            destination,
                            caps,
                            on_progress,
                            cancel,
                        )
                        .await?;
                        (result.moved.len(), result.failed.len())
                    }
                };
                imap_client::sync(session).await?;
                mailbox_found += uids.len();
                mailbox_affected += affected;
                mailbox_failed += failed;
                if affected == 0 {
                    drained = true;
                    break;
                }
            }
            if !drained {
                totals.skipped.push(mbox.clone());
            }
            totals.found += mailbox_found;
            totals.affected += mailbox_affected;
            totals.failed += mailbox_failed;
            if mailbox_found > 0 {
                totals.mailboxes.push(SweepMailboxTally {
                    mailbox: mbox.clone(),
                    found: mailbox_found,
                    affected: mailbox_affected,
                    failed: mailbox_failed,
                });
            }
        }
        Ok(totals)
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
        if let Err(error) = Self::require_disposal_path(mode, trash.as_deref(), &caps) {
            session.release().await;
            return Err(error);
        }

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

        let totals = self
            .matching_sweep(
                session,
                account,
                DeleteSelector::ListId(list_id.to_string()),
                &mailboxes,
                SweepAction::Delete {
                    trash: trash.as_deref(),
                    allow_permanent_fallback: false,
                },
                &caps,
                on_progress,
                cancel,
            )
            .await?;

        Ok(DeleteListIdResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            list_id: list_id.to_string(),
            found: totals.found,
            deleted: totals.affected,
            failed: totals.failed,
            mailboxes: delete_tallies(totals.mailboxes),
            skipped: totals.skipped,
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
        if let Err(error) = Self::require_disposal_path(mode, trash.as_deref(), &caps) {
            session.release().await;
            return Err(error);
        }
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;
        if !uids.is_empty() {
            self.fence_header_cache_mutation(account).await;
        }
        // No silent permanent escalation: a failed Trash MOVE is reported as
        // failed UIDs, never upgraded to EXPUNGE the caller did not request.
        let result = imap_client::bulk_delete_messages_with_policy(
            session.session(),
            uids,
            trash.as_deref(),
            &caps,
            false,
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

    /// Delete all messages from an exact sender identity (email + display
    /// name, as returned by `top_senders`/`top_subscriptions` rows).
    ///
    /// `mailbox: None` means the account-wide mutation plan, matching
    /// `delete_list_id`. Discovery re-finds and confirms the identity live in
    /// each mailbox, so no sample UID or UIDVALIDITY guard is needed.
    pub async fn delete_by_sender(
        &self,
        mailbox: Option<&str>,
        account: &str,
        email: &str,
        name: &str,
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<DeleteBySenderResponse> {
        let email = email.trim();
        if email.is_empty() {
            return Err(AgentmailError::Other(
                "sender email is required (use address from a top_senders/top_subscriptions row)"
                    .to_string(),
            ));
        }
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await;
        if let Err(error) = Self::require_disposal_path(mode, trash.as_deref(), &caps) {
            session.release().await;
            return Err(error);
        }

        let sender_display = if name.is_empty() {
            email.to_string()
        } else {
            format!("{name} <{email}>")
        };

        let search_mailboxes = match mailbox {
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

        let totals = self
            .matching_sweep(
                session,
                account,
                DeleteSelector::Sender {
                    email: email.to_string(),
                    name: name.to_string(),
                    bulk_only: false,
                    list_id: None,
                },
                &search_mailboxes,
                SweepAction::Delete {
                    trash: trash.as_deref(),
                    allow_permanent_fallback: false,
                },
                &caps,
                on_progress,
                cancel,
            )
            .await?;

        Ok(DeleteBySenderResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            sender: sender_display,
            found: totals.found,
            deleted: totals.affected,
            failed: totals.failed,
            mailboxes: delete_tallies(totals.mailboxes),
            skipped: totals.skipped,
            permanent: mode == DeleteMode::Permanent,
        })
    }

    // -----------------------------------------------------------------
    // Move
    // -----------------------------------------------------------------

    /// Shared front half of the bulk move flows: validates the destination,
    /// resolves the mailbox scope (excluding the destination account-wide),
    /// and runs the matching sweep with a Move action.
    async fn move_sweep(
        &self,
        mailbox: Option<&str>,
        account: &str,
        selector: DeleteSelector,
        destination: &str,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<SweepTotals> {
        let destination = destination.trim();
        if destination.is_empty() {
            return Err(AgentmailError::Other("destination is required".to_string()));
        }
        if let Some(mbox) = mailbox
            && mbox.eq_ignore_ascii_case(destination)
        {
            return Err(AgentmailError::Other(
                "destination equals the source mailbox; nothing to move".to_string(),
            ));
        }
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        if !caps.has_move() && !caps.has_uidplus() {
            session.release().await;
            return Err(AgentmailError::Other(
                "server supports neither MOVE nor UIDPLUS; cannot move messages safely".to_string(),
            ));
        }
        // Validate the destination exists before any mutation (same check as
        // move_message).
        let names = imap_client::list_mailbox_names(session.session()).await?;
        if !names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(destination))
        {
            session.release().await;
            return Err(AgentmailError::Other(format!(
                "Destination mailbox '{destination}' does not exist"
            )));
        }
        let mailboxes: Vec<String> = match mailbox {
            Some(mbox) => vec![mbox.to_string()],
            None => self
                .account_scan_mailboxes(
                    account,
                    session.session(),
                    scan_plan::ScanPurpose::Mutation,
                )
                .await?
                .into_iter()
                // Never sweep the destination itself: the just-moved messages
                // still match the selector and would be "moved" onto
                // themselves pass after pass.
                .filter(|mbox| !mbox.eq_ignore_ascii_case(destination))
                .collect(),
        };
        self.matching_sweep(
            session,
            account,
            selector,
            &mailboxes,
            SweepAction::Move { destination },
            &caps,
            on_progress,
            cancel,
        )
        .await
    }

    /// Move every message with an exact List-Id to `destination` in one
    /// operation — e.g. statements or a newsletter into an archive folder.
    /// `mailbox: None` sweeps the account-wide mutation plan (destination
    /// excluded). Discovery matches `delete_list_id`: server search, cached
    /// projection fast-path, and a live exact-List-Id confirm before any move.
    pub async fn move_list_id(
        &self,
        mailbox: Option<&str>,
        account: &str,
        list_id: &str,
        destination: &str,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<MoveListIdResponse> {
        let totals = self
            .move_sweep(
                mailbox,
                account,
                DeleteSelector::ListId(list_id.to_string()),
                destination,
                on_progress,
                cancel,
            )
            .await?;
        Ok(MoveListIdResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            list_id: list_id.to_string(),
            destination: destination.trim().to_string(),
            found: totals.found,
            moved: totals.affected,
            failed: totals.failed,
            mailboxes: move_tallies(totals.mailboxes),
            skipped: totals.skipped,
        })
    }

    /// Move every message from an exact sender identity (email + display
    /// name, as returned by ranking rows) to `destination` in one operation.
    /// `mailbox: None` sweeps the account-wide mutation plan (destination
    /// excluded). Discovery confirms the identity live, like
    /// `delete_by_sender`.
    #[allow(clippy::too_many_arguments)]
    pub async fn move_by_sender(
        &self,
        mailbox: Option<&str>,
        account: &str,
        email: &str,
        name: &str,
        destination: &str,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<MoveBySenderResponse> {
        let email = email.trim();
        if email.is_empty() {
            return Err(AgentmailError::Other(
                "sender email is required (use address from a top_senders/top_subscriptions row)"
                    .to_string(),
            ));
        }
        let sender_display = if name.is_empty() {
            email.to_string()
        } else {
            format!("{name} <{email}>")
        };
        let totals = self
            .move_sweep(
                mailbox,
                account,
                DeleteSelector::Sender {
                    email: email.to_string(),
                    name: name.to_string(),
                    bulk_only: false,
                    list_id: None,
                },
                destination,
                on_progress,
                cancel,
            )
            .await?;
        Ok(MoveBySenderResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            sender: sender_display,
            destination: destination.trim().to_string(),
            found: totals.found,
            moved: totals.affected,
            failed: totals.failed,
            mailboxes: move_tallies(totals.mailboxes),
            skipped: totals.skipped,
        })
    }

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
    /// unsubscribe and optionally delete matching messages across account
    /// storage mailboxes.
    ///
    /// Cleanup runs only when `options.cleanup` is present; its `when`,
    /// `identity`, and `deletion` axes are orthogonal policies (see
    /// [`CleanupPolicy`]).
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
        let target = match imap_client::fetch_unsubscribe_target(
            session.session(),
            mailbox,
            uid,
            options.expected_uid_validity,
            cancel,
        )
        .await
        {
            Ok(target) => target,
            Err(error) => {
                if matches!(error, AgentmailError::MessageNotFound(_)) {
                    // The ranking sample went stale — deleted by another
                    // client, which Yahoo/AOL never surface (UIDNEXT
                    // unchanged, EXISTS untrustworthy). Prune the row so the
                    // next ranking call offers a live sample, and hand back
                    // the healthy session (the server answered cleanly).
                    if let Some(config) = self.pool.account_config(account) {
                        self.header_cache
                            .prune_uid(account, config, mailbox, uid)
                            .await;
                    }
                    session.release().await;
                }
                return Err(error);
            }
        };
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

        let Some(cleanup) = options.cleanup else {
            return Ok(response);
        };
        if !cleanup_policy_allows(cleanup, response.unsubscribed.success) {
            response.cleanup_skipped_reason = Some(
                "Matching-message cleanup was skipped because the unsubscribe attempt failed and cleanup.when was not \"always\"."
                    .to_string(),
            );
            return Ok(response);
        }

        let identity = match select_unsubscribe_cleanup_identity(
            headers.list_id.as_deref(),
            headers.has_single_list_id(),
            response.list_id_authenticated,
            &target_email,
            cleanup.identity,
        ) {
            Ok(identity) => identity,
            Err(CleanupIdentityError::UnauthenticatedListId) => {
                response.cleanup_skipped_reason = Some(
                    "Matching-message cleanup was skipped: the sender's DKIM signature does not cover the List-Id header (List-Id is spoofable, so an unauthenticated value must not select an account-wide delete), and cleanup.identity was \"listIdOnly\". To clean up: use cleanup.identity \"listIdOrSender\" to delete this exact sender's bulk mail scoped to this List-Id, use delete_list_id with an explicitly chosen listId, or use delete_by_sender."
                        .to_string(),
                );
                return Ok(response);
            }
            Err(CleanupIdentityError::NoUsableListId) => {
                response.cleanup_skipped_reason = Some(
                    "Matching-message cleanup was skipped: the message carries no single usable List-Id, and cleanup.identity was \"listIdOnly\". To clean up: use cleanup.identity \"listIdOrSender\" to delete this exact sender's bulk mail, or use delete_by_sender."
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

        let mode = cleanup.mode();
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await;
        if caps.is_gmail() && trash.is_none() {
            session.release().await;
            response.cleanup_skipped_reason = Some(
                "Matching-message cleanup was skipped because Gmail Trash could not be resolved; in-place EXPUNGE only removes a label and is not a permanent delete."
                    .to_string(),
            );
            return Ok(response);
        }
        if mode == DeleteMode::TrashFirst && trash.is_none() && !cleanup.allow_permanent_fallback()
        {
            session.release().await;
            response.cleanup_skipped_reason = Some(
                "Matching-message cleanup was skipped because no Trash mailbox was available and cleanup.deletion did not permit a permanent fallback (\"trashThenPermanent\" or \"permanent\")."
                    .to_string(),
            );
            return Ok(response);
        }

        let all_mailboxes = self
            .account_scan_mailboxes(account, session.session(), scan_plan::ScanPurpose::Mutation)
            .await?;
        let selector = match &identity {
            CleanupIdentity::ListId { normalized, .. } => {
                DeleteSelector::ListId(normalized.clone())
            }
            CleanupIdentity::Sender { list_id } => DeleteSelector::Sender {
                email: target_email.clone(),
                name: target_name.clone(),
                bulk_only: true,
                list_id: list_id.clone(),
            },
        };
        // A TrashFirst cleanup with no resolvable Trash is already a permanent
        // fallback before the sweep runs; carry that seed into the result.
        let seeded_trash_fallback = mode == DeleteMode::TrashFirst && trash.is_none();
        let totals = self
            .matching_sweep(
                session,
                account,
                selector,
                &all_mailboxes,
                SweepAction::Delete {
                    trash: trash.as_deref(),
                    allow_permanent_fallback: cleanup.allow_permanent_fallback(),
                },
                &caps,
                on_progress,
                cancel,
            )
            .await?;
        let trash_fallback = totals.trash_fallback || seeded_trash_fallback;

        let (matched_by, list_id) = match identity {
            CleanupIdentity::ListId { raw, .. } => ("list-id", Some(raw)),
            // The constrained fallback reports the normalized List-Id it was
            // scoped to; matched_by distinguishes it from the authenticated
            // List-Id match.
            CleanupIdentity::Sender {
                list_id: Some(normalized),
            } => ("exact-sender-list-id-fallback", Some(normalized)),
            CleanupIdentity::Sender { list_id: None } => ("exact-sender-fallback", None),
        };
        let complete = totals.skipped.is_empty() && totals.failed == 0;
        response.matching_messages = Some(MatchingMessagesResult {
            matched_by: matched_by.to_string(),
            sender: sender_display,
            list_id,
            found: totals.found,
            deleted: totals.affected,
            failed: totals.failed,
            mailboxes: delete_tallies(totals.mailboxes),
            skipped: totals.skipped,
            // Gmail's safe provider-specific interpretation of Permanent is
            // a move to Trash: in-place UID EXPUNGE only removes a label.
            permanent: (mode == DeleteMode::Permanent && !caps.is_gmail()) || trash_fallback,
            trash_fallback,
            complete,
        });
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

    /// Refuse a trash-first delete when no Trash mailbox is resolvable.
    /// Without this guard the chunked delete silently escalates to
    /// flag+EXPUNGE (permanent) on UIDPLUS servers while the response still
    /// reports `permanent: false`. Permanent mode needs no Trash — except on
    /// Gmail, where an unresolvable Trash breaks both modes because in-place
    /// EXPUNGE only removes a label.
    fn require_disposal_path(
        mode: DeleteMode,
        trash: Option<&str>,
        caps: &imap_client::ServerCaps,
    ) -> Result<()> {
        if mode == DeleteMode::TrashFirst && trash.is_none() {
            if caps.is_gmail() {
                return Err(AgentmailError::Other(
                    "Gmail Trash could not be resolved; in-place EXPUNGE only removes a label and is not a delete".to_string(),
                ));
            }
            return Err(AgentmailError::Other(
                "no Trash mailbox could be resolved for a trash-first delete; retry with permanent=true to delete irreversibly".to_string(),
            ));
        }
        Ok(())
    }

    /// Invalidate cached layout after a mailbox mutation.
    fn invalidate_mailbox_catalog(&self, account: &str) {
        self.mailbox_catalog.invalidate(account);
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// What a delete sweep matches. The discovery predicate is the only thing that
/// differs across the list-id, exact-sender, and unsubscribe-cleanup deletes;
/// everything else (windowed draining, chunked expunge, per-mailbox tallying,
/// UID-Mode entry) is shared in `delete_sweep`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeleteSelector {
    /// Messages whose List-Id matches (exact). `exact_list_id_uids` normalizes
    /// internally, so the raw header value is accepted here.
    ListId(String),
    /// Messages from an exact sender identity. `bulk_only` restricts to mail
    /// that also carries a List-Unsubscribe header (unsubscribe-cleanup
    /// semantics); otherwise every message from the identity matches
    /// (delete-by-sender). `list_id` further scopes bulk mail to one
    /// normalized List-Id — the pertinence constraint for the unsubscribe
    /// sender fallback, so sibling lists from the same sender are untouched.
    Sender {
        email: String,
        name: String,
        bulk_only: bool,
        list_id: Option<String>,
    },
}

/// What a matching sweep does with each discovered batch. Discovery, the
/// windowed drain loop, UID-Mode entry, and per-mailbox tallying are shared;
/// only this final action differs.
#[derive(Debug, Clone, Copy)]
enum SweepAction<'a> {
    /// Move to Trash (or expunge, per policy) — the delete flows.
    Delete {
        trash: Option<&'a str>,
        allow_permanent_fallback: bool,
    },
    /// Move to an ordinary destination mailbox — the bulk move flows.
    Move { destination: &'a str },
}

/// Per-mailbox tally of one sweep. `affected` is "deleted" or "moved"
/// depending on the action; wrappers map it to their wire field.
#[derive(Debug)]
struct SweepMailboxTally {
    mailbox: String,
    found: usize,
    affected: usize,
    failed: usize,
}

/// Aggregated outcome of a matching sweep across one or more mailboxes.
#[derive(Debug, Default)]
struct SweepTotals {
    found: usize,
    affected: usize,
    failed: usize,
    mailboxes: Vec<SweepMailboxTally>,
    skipped: Vec<String>,
    trash_fallback: bool,
}

fn delete_tallies(tallies: Vec<SweepMailboxTally>) -> Vec<PerMailboxDeleteResult> {
    tallies
        .into_iter()
        .map(|tally| PerMailboxDeleteResult {
            mailbox: tally.mailbox,
            found: tally.found,
            deleted: tally.affected,
            failed: tally.failed,
        })
        .collect()
}

fn move_tallies(tallies: Vec<SweepMailboxTally>) -> Vec<PerMailboxMoveResult> {
    tallies
        .into_iter()
        .map(|tally| PerMailboxMoveResult {
            mailbox: tally.mailbox,
            found: tally.found,
            moved: tally.affected,
            failed: tally.failed,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum CleanupIdentity {
    ListId {
        raw: String,
        normalized: String,
    },
    /// Exact-sender fallback. `list_id` carries the target's single normalized
    /// List-Id when the message has one that DKIM did not authenticate: the
    /// sender identity is verified, so conjoining it with that List-Id scopes
    /// the delete to the one list actually unsubscribed from without trusting
    /// the (spoofable) List-Id alone. `None` when the message carries no
    /// usable List-Id at all — sender + bulk-mail is then the only criterion.
    Sender {
        list_id: Option<String>,
    },
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
    identity_mode: CleanupIdentityMode,
) -> std::result::Result<CleanupIdentity, CleanupIdentityError> {
    let normalized = has_single_list_id
        .then_some(list_id)
        .flatten()
        .and_then(normalize_list_id);

    match (normalized, list_id_authenticated) {
        (Some(normalized), true) => Ok(CleanupIdentity::ListId {
            raw: list_id.unwrap_or_default().to_string(),
            normalized,
        }),
        (normalized, _)
            if identity_mode == CleanupIdentityMode::ListIdOrSender && !target_email.is_empty() =>
        {
            Ok(CleanupIdentity::Sender {
                list_id: normalized,
            })
        }
        (Some(_), _) => Err(CleanupIdentityError::UnauthenticatedListId),
        (None, _) => Err(CleanupIdentityError::NoUsableListId),
    }
}

/// The two invariant checks. The old cross-field combination rules are gone:
/// the nested `CleanupPolicy` makes contradictory flag sets unrepresentable.
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
    Ok(())
}

fn cleanup_policy_allows(cleanup: CleanupPolicy, unsubscribe_succeeded: bool) -> bool {
    unsubscribe_succeeded || cleanup.when == CleanupWhen::Always
}

/// From a set of candidate UIDs, fetch FROM + List-Unsubscribe/Post headers and
/// return only those that match the exact sender AND have either List-Unsubscribe
/// or List-Unsubscribe-Post (i.e. bulk/marketing mail).
/// Confirm which candidate UIDs actually carry the exact List-Id. IMAP HEADER
/// search is substring-only (and enumeration candidates are everything), so
/// this fetch is the authority before any deletion; stale UIDs simply do not
/// come back.
async fn confirm_exact_list_id<T>(
    session: &mut async_imap::Session<T>,
    candidates: &[u32],
    list_id: &str,
    cancel: Option<&CancelFn>,
) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    Ok(
        imap_client::fetch_list_ids_for_uids_cancellable(session, candidates, cancel)
            .await?
            .into_iter()
            .filter(|(_, id)| id.as_deref().is_some_and(|v| list_id_matches(list_id, v)))
            .map(|(uid, _)| uid)
            .collect(),
    )
}

async fn filter_sender_bulk_mail<T>(
    session: &mut async_imap::Session<T>,
    candidate_uids: &[u32],
    target_email: &str,
    target_name: &str,
    constrain_list_id: Option<&str>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    let mut exact = Vec::new();
    for chunk in candidate_uids.chunks(1000) {
        imap_client::check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        // Full header block, not HEADER.FIELDS: Yahoo/AOL filter the
        // List-Unsubscribe pair out of HEADER.FIELDS responses (see
        // docs/standards/imap/yahoo-aol-quirks.md), which would silently
        // disqualify every candidate here. Candidate sets are one sender's
        // mail in one mailbox, so the extra bytes are negligible next to a
        // wrong bulk-mail classification on a deletion path.
        let fetched =
            imap_client::timed_uid_fetch_collect_pub(session, &uid_set, "(UID BODY.PEEK[HEADER])")
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

            // Pertinence constraint: when the cleanup identity carries a
            // List-Id, only that list's mail qualifies.
            if !row_list_id_matches(&header_str, constrain_list_id) {
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

/// Outcome of a page-sample Subject fetch.
struct SampleSubjects {
    /// Decoded Subject per (mailbox, uid) that the server returned a row for.
    subjects: hashbrown::HashMap<(String, u32), String>,
    /// Samples whose mailbox FETCH **succeeded** yet returned no row for the
    /// UID — the strongest available deleted-message signal on providers
    /// where external deletions are otherwise invisible (Yahoo/AOL advance
    /// neither UIDNEXT nor a trustworthy EXISTS). Never populated from a
    /// failed EXAMINE or FETCH: an outage must not masquerade as deletion.
    missing: Vec<(String, u32)>,
}

/// Fetch the decoded Subject of each ranking sample so a page can say WHAT a
/// list or subscription actually is ("Your July statement is ready") instead
/// of only who sent it. One EXAMINE plus one bounded UID FETCH per distinct
/// sample mailbox, on the session the ranking already holds — never more than
/// a page's worth of UIDs. Best-effort by design: a stale sample, a fetch
/// failure, or a missing Subject header simply yields no entry, and the row
/// ships without a subject. Subjects are returned transiently and never
/// persisted (the ranking cache deliberately stores no subjects).
async fn sample_subjects<T>(
    session: &mut async_imap::Session<T>,
    samples: &[MailboxMessageIdentity],
    cancel: Option<&CancelFn>,
) -> SampleSubjects
where
    T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    let mut by_mailbox: hashbrown::HashMap<&str, Vec<u32>> = hashbrown::HashMap::new();
    for sample in samples {
        by_mailbox
            .entry(sample.mailbox.as_str())
            .or_default()
            .push(sample.uid);
    }

    let mut subjects = hashbrown::HashMap::new();
    let mut missing = Vec::new();
    for (mailbox, uids) in by_mailbox {
        if imap_client::check_cancel(cancel).is_err() {
            break;
        }
        if imap_client::examine(session, mailbox).await.is_err() {
            continue;
        }
        let uid_set: String = uids
            .iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let Ok(fetched) = imap_client::timed_uid_fetch_collect_pub(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (Subject)])",
        )
        .await
        else {
            continue;
        };
        // Which requested UIDs the server acknowledged with a row at all —
        // a row with no Subject header still proves the message is alive.
        let mut returned: hashbrown::HashSet<u32> = hashbrown::HashSet::new();
        for item in fetched {
            let Ok(fetch) = item else { continue };
            let Some(uid) = fetch.uid else { continue };
            returned.insert(uid);
            if let Some(subject) = fetch.header().and_then(parser::parse_subject) {
                subjects.insert((mailbox.to_string(), uid), subject);
            }
        }
        missing.extend(
            uids.iter()
                .filter(|uid| !returned.contains(*uid))
                .map(|uid| (mailbox.to_string(), *uid)),
        );
    }
    SampleSubjects { subjects, missing }
}

/// Whether a message's header block satisfies an optional normalized List-Id
/// constraint. `None` imposes nothing; `Some` requires the header to carry a
/// List-Id that normalizes to exactly that value — a missing or different
/// List-Id fails, so a sender-fallback cleanup scoped to one list can never
/// take a sibling list's mail with it.
fn row_list_id_matches(header_str: &str, constrain_list_id: Option<&str>) -> bool {
    let Some(want) = constrain_list_id else {
        return true;
    };
    imap_client::extract_header_value_pub(header_str, "List-Id")
        .as_deref()
        .and_then(normalize_list_id)
        .is_some_and(|normalized| normalized == want)
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
                auth: crate::config::AuthMethod::Password,
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
            cleanup: None,
        }
    }

    #[test]
    fn plain_unsubscribe_and_every_cleanup_policy_pass_validation() {
        // The nested policy makes contradictory combinations unrepresentable,
        // so validation accepts cleanup: None and every enum combination alike.
        assert!(validate_unsubscribe_options(unsubscribe_options()).is_ok());
        for when in [CleanupWhen::AfterSuccess, CleanupWhen::Always] {
            for identity in [
                CleanupIdentityMode::ListIdOnly,
                CleanupIdentityMode::ListIdOrSender,
            ] {
                for deletion in [
                    CleanupDeletion::Trash,
                    CleanupDeletion::TrashThenPermanent,
                    CleanupDeletion::Permanent,
                ] {
                    let mut options = unsubscribe_options();
                    options.cleanup = Some(CleanupPolicy {
                        when,
                        identity,
                        deletion,
                    });
                    assert!(validate_unsubscribe_options(options).is_ok());
                }
            }
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
        let after_success = CleanupPolicy::default();
        assert!(cleanup_policy_allows(after_success, true));
        assert!(!cleanup_policy_allows(after_success, false));

        let always = CleanupPolicy {
            when: CleanupWhen::Always,
            ..CleanupPolicy::default()
        };
        assert!(cleanup_policy_allows(always, true));
        assert!(cleanup_policy_allows(always, false));
    }

    #[test]
    fn cleanup_deletion_maps_to_mode_and_fallback() {
        let policy = |deletion| CleanupPolicy {
            deletion,
            ..CleanupPolicy::default()
        };
        assert_eq!(
            policy(CleanupDeletion::Trash).mode(),
            DeleteMode::TrashFirst
        );
        assert!(!policy(CleanupDeletion::Trash).allow_permanent_fallback());
        assert_eq!(
            policy(CleanupDeletion::TrashThenPermanent).mode(),
            DeleteMode::TrashFirst
        );
        assert!(policy(CleanupDeletion::TrashThenPermanent).allow_permanent_fallback());
        assert_eq!(
            policy(CleanupDeletion::Permanent).mode(),
            DeleteMode::Permanent
        );
        assert!(!policy(CleanupDeletion::Permanent).allow_permanent_fallback());
    }

    #[test]
    fn cleanup_identity_requires_dkim_authenticated_list_id() {
        let list_id = Some("Newsletter <news.example.com>");
        let strict = CleanupIdentityMode::ListIdOnly;
        let fallback = CleanupIdentityMode::ListIdOrSender;
        assert_eq!(
            select_unsubscribe_cleanup_identity(list_id, true, true, "sender@example.com", strict),
            Ok(CleanupIdentity::ListId {
                raw: "Newsletter <news.example.com>".to_string(),
                normalized: "news.example.com".to_string(),
            })
        );
        assert_eq!(
            select_unsubscribe_cleanup_identity(list_id, true, false, "sender@example.com", strict),
            Err(CleanupIdentityError::UnauthenticatedListId)
        );
        assert_eq!(
            select_unsubscribe_cleanup_identity(list_id, false, true, "sender@example.com", strict),
            Err(CleanupIdentityError::NoUsableListId)
        );
        assert_eq!(
            select_unsubscribe_cleanup_identity(None, false, false, "", fallback),
            Err(CleanupIdentityError::NoUsableListId)
        );
    }

    #[test]
    fn sender_fallback_is_scoped_to_the_unauthenticated_list_id() {
        // The pertinence ladder: an unauthenticated List-Id must not select an
        // account-wide List-Id delete on its own (spoofable), but conjoined
        // with the verified sender it scopes the fallback to the one list the
        // user actually unsubscribed from.
        let fallback = CleanupIdentityMode::ListIdOrSender;
        assert_eq!(
            select_unsubscribe_cleanup_identity(
                Some("Newsletter <news.example.com>"),
                true,
                false,
                "sender@example.com",
                fallback
            ),
            Ok(CleanupIdentity::Sender {
                list_id: Some("news.example.com".to_string()),
            })
        );
        // No usable List-Id at all → sender + bulk-mail is the only criterion.
        assert_eq!(
            select_unsubscribe_cleanup_identity(None, false, false, "sender@example.com", fallback),
            Ok(CleanupIdentity::Sender { list_id: None })
        );
        // Multiple List-Ids (has_single = false) → ambiguous, no constraint.
        assert_eq!(
            select_unsubscribe_cleanup_identity(
                Some("Newsletter <news.example.com>"),
                false,
                false,
                "sender@example.com",
                fallback
            ),
            Ok(CleanupIdentity::Sender { list_id: None })
        );
    }

    #[test]
    fn row_list_id_constraint_is_exact_and_normalized() {
        let matching = "List-Id: Newsletter <news.example.com>\r\nFrom: a@b.c\r\n";
        let sibling = "List-Id: Other <other.example.com>\r\nFrom: a@b.c\r\n";
        let missing = "From: a@b.c\r\n";
        // No constraint → everything passes (plain sender fallback).
        assert!(row_list_id_matches(matching, None));
        assert!(row_list_id_matches(missing, None));
        // Constrained → only the exact normalized List-Id passes; sibling
        // lists and List-Id-free mail from the same sender are protected.
        assert!(row_list_id_matches(matching, Some("news.example.com")));
        assert!(!row_list_id_matches(sibling, Some("news.example.com")));
        assert!(!row_list_id_matches(missing, Some("news.example.com")));
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

    #[test]
    fn builder_cache_choices_control_persistence() {
        let disabled = Agentmail::builder(Config::empty()).disable_cache().build();
        assert!(
            !disabled.header_cache.is_persistent(),
            "disable_cache() must turn off persistence regardless of env"
        );

        let dir = std::env::temp_dir().join("agentmail-builder-test");
        let explicit = Agentmail::builder(Config::empty()).cache_dir(&dir).build();
        assert!(
            explicit.header_cache.is_persistent(),
            "cache_dir() must enable persistence at the given directory"
        );
    }

    #[test]
    fn builder_login_cooldown_reaches_the_pool_gate() {
        let mail = Agentmail::builder(Config::empty())
            .login_cooldown(std::time::Duration::from_secs(100))
            .build();
        mail.pool.note_login_rate_limit("aol");
        let remaining = mail
            .pool
            .login_cooldown_remaining("aol")
            .expect("cooldown armed");
        assert!(
            remaining <= std::time::Duration::from_secs(100),
            "custom cooldown caps the gate: {remaining:?}"
        );
        assert!(
            remaining > std::time::Duration::from_secs(95),
            "custom cooldown is actually applied (not the 300s default): {remaining:?}"
        );
    }

    /// A scripted IMAP server for driving `delete_sweep_loop` over a duplex
    /// stream. Dispatches on command substrings and scripts two drain passes:
    /// the first SELECT/SEARCH finds messages, the second finds none. Returns
    /// every received command line for shape assertions.
    fn scripted_sweep_server(
        server_stream: tokio::io::DuplexStream,
        first_search: &'static str,
        fetch_reply: fn(&str) -> String,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();
            let mut selects = 0u32;
            let mut searches = 0u32;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                let tag = line.split_whitespace().next().unwrap().to_string();
                commands.push(line.clone());
                let reply = if line.contains(" LOGIN ") {
                    format!("{tag} OK LOGIN completed\r\n")
                } else if line.contains("SELECT") {
                    selects += 1;
                    // Pass 2 re-selects and sees the emptied mailbox.
                    let exists = if selects == 1 { 2 } else { 0 };
                    format!(
                        "* {exists} EXISTS\r\n* OK [UIDVALIDITY 9] UIDs valid\r\n* OK [UIDNEXT 100] next\r\n{tag} OK [READ-WRITE] SELECT completed\r\n"
                    )
                } else if line.contains("UID SEARCH") {
                    searches += 1;
                    let hits = if searches == 1 { first_search } else { "" };
                    format!("* SEARCH{hits}\r\n{tag} OK SEARCH completed\r\n")
                } else if line.contains("UID FETCH") {
                    fetch_reply(&tag)
                } else if line.contains("UID STORE") {
                    format!("{tag} OK STORE completed\r\n")
                } else if line.contains("UID EXPUNGE") {
                    format!("* 1 EXPUNGE\r\n{tag} OK EXPUNGE completed\r\n")
                } else if line.contains("UID MOVE") {
                    format!("* 1 EXPUNGE\r\n{tag} OK MOVE completed\r\n")
                } else if line.contains("NOOP") {
                    format!("{tag} OK NOOP completed\r\n")
                } else {
                    panic!("unexpected command: {line:?}");
                };
                writer.write_all(reply.as_bytes()).await.unwrap();
            }
            commands
        })
    }

    async fn scripted_sweep_session(
        first_search: &'static str,
        fetch_reply: fn(&str) -> String,
    ) -> (
        async_imap::Session<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server = scripted_sweep_server(server_stream, first_search, fetch_reply);
        let client = async_imap::Client::new(client_stream);
        let session = client
            .login("test-user", "test-password")
            .await
            .map_err(|(error, _)| error)
            .unwrap();
        (session, server)
    }

    /// End-to-end List-Id sweep against a scripted server: discovery searches,
    /// the confirm fetch keeps only exact-List-Id matches, the delete issues
    /// UID STORE + UID EXPUNGE (UIDPLUS, no Trash), and the second drain pass
    /// finds nothing and terminates.
    #[tokio::test]
    async fn delete_sweep_list_id_confirms_deletes_and_drains() {
        let (mut session, server) = scripted_sweep_session(" 5 7", |tag| {
            // Confirm fetch: UID 5 carries the exact List-Id, UID 7 a sibling
            // list — only 5 may be deleted.
            let target = "List-Id: News <news.example.com>\r\n\r\n";
            let sibling = "List-Id: Other <other.example.com>\r\n\r\n";
            format!(
                "* 1 FETCH (UID 5 BODY[HEADER.FIELDS (LIST-ID)] {{{}}}\r\n{target})\r\n* 2 FETCH (UID 7 BODY[HEADER.FIELDS (LIST-ID)] {{{}}}\r\n{sibling})\r\n{tag} OK FETCH completed\r\n",
                target.len(),
                sibling.len()
            )
        })
        .await;

        let mk = Agentmail::new(Config::empty());
        let caps = imap_client::ServerCaps::from_strings(["UIDPLUS".to_string()]);
        let totals = mk
            .matching_sweep_loop(
                &mut session,
                "test-account",
                &DeleteSelector::ListId("news.example.com".to_string()),
                &["INBOX".to_string()],
                SweepAction::Delete {
                    trash: None,
                    allow_permanent_fallback: false,
                },
                &caps,
                None,
                None,
            )
            .await
            .expect("scripted sweep succeeds");

        assert_eq!(totals.found, 1, "only the confirmed List-Id match counts");
        assert_eq!(totals.affected, 1);
        assert_eq!(totals.failed, 0);
        assert!(totals.skipped.is_empty());
        assert_eq!(totals.mailboxes.len(), 1);
        assert_eq!(totals.mailboxes[0].mailbox, "INBOX");

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        let joined = commands.concat();
        assert!(
            joined.contains("UID STORE 5 +FLAGS (\\Deleted)"),
            "deletes exactly the confirmed UID: {commands:?}"
        );
        assert!(
            joined.contains("UID EXPUNGE 5"),
            "expunges via targeted UIDPLUS: {commands:?}"
        );
        assert!(
            !joined.contains("UID STORE 5,7") && !joined.contains("UID EXPUNGE 5,7"),
            "the sibling-list UID must never reach the delete: {commands:?}"
        );
        assert_eq!(
            commands.iter().filter(|c| c.contains("SELECT")).count(),
            2,
            "drain loop re-selects once and stops on the empty pass: {commands:?}"
        );
    }

    /// Ranking pages enrich each row with its sample's decoded Subject: one
    /// EXAMINE + one UID FETCH per mailbox, RFC 2047 encoded-words decoded,
    /// and a stale sample (no FETCH row) simply yields no subject.
    #[tokio::test]
    async fn sample_subjects_fetches_and_decodes_per_mailbox() {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                let tag = line.split_whitespace().next().unwrap().to_string();
                commands.push(line.clone());
                let reply = if line.contains(" LOGIN ") {
                    format!("{tag} OK LOGIN completed\r\n")
                } else if line.contains("EXAMINE") {
                    format!(
                        "* 3 EXISTS\r\n* OK [UIDVALIDITY 9] UIDs valid\r\n* OK [UIDNEXT 100] next\r\n{tag} OK [READ-ONLY] EXAMINE completed\r\n"
                    )
                } else if line.contains("UID FETCH") {
                    // UID 5: plain subject. UID 7: RFC 2047 encoded ("Résumé").
                    // UID 9 (requested) has no row — a stale sample.
                    let plain = "Subject: Your July statement is ready\r\n\r\n";
                    let encoded = "Subject: =?UTF-8?B?UsOpc3Vtw6k=?=\r\n\r\n";
                    format!(
                        "* 1 FETCH (UID 5 BODY[HEADER.FIELDS (SUBJECT)] {{{}}}\r\n{plain})\r\n* 2 FETCH (UID 7 BODY[HEADER.FIELDS (SUBJECT)] {{{}}}\r\n{encoded})\r\n{tag} OK FETCH completed\r\n",
                        plain.len(),
                        encoded.len()
                    )
                } else {
                    panic!("unexpected command: {line:?}");
                };
                writer.write_all(reply.as_bytes()).await.unwrap();
            }
            commands
        });
        let client = async_imap::Client::new(client_stream);
        let mut session = client
            .login("test-user", "test-password")
            .await
            .map_err(|(error, _)| error)
            .unwrap();

        let samples = vec![
            MailboxMessageIdentity {
                mailbox: "INBOX".to_string(),
                uid_validity: 9,
                uid: 5,
            },
            MailboxMessageIdentity {
                mailbox: "INBOX".to_string(),
                uid_validity: 9,
                uid: 7,
            },
            MailboxMessageIdentity {
                mailbox: "INBOX".to_string(),
                uid_validity: 9,
                uid: 9,
            },
        ];
        let SampleSubjects { subjects, missing } =
            sample_subjects(&mut session, &samples, None).await;

        assert_eq!(
            subjects.get(&("INBOX".to_string(), 5)).map(String::as_str),
            Some("Your July statement is ready")
        );
        assert_eq!(
            subjects.get(&("INBOX".to_string(), 7)).map(String::as_str),
            Some("Résumé"),
            "RFC 2047 encoded-words are decoded"
        );
        assert!(
            !subjects.contains_key(&("INBOX".to_string(), 9)),
            "a stale sample yields no subject instead of an error"
        );
        assert_eq!(
            missing,
            vec![("INBOX".to_string(), 9)],
            "a UID absent from a SUCCESSFUL fetch is reported as deleted so \
             the caller can prune it; live UIDs are not"
        );

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains("UID FETCH"))
                .count(),
            1,
            "one bounded fetch per mailbox covers the whole page: {commands:?}"
        );
    }

    /// End-to-end bulk MOVE sweep: same discovery + confirm as the delete
    /// sweep, but the batch is moved with UID MOVE (server advertises MOVE)
    /// instead of being expunged — the move_list_id/move_by_sender data path.
    #[tokio::test]
    async fn matching_sweep_moves_confirmed_uids_with_uid_move() {
        let (mut session, server) = scripted_sweep_session(" 5 7", |tag| {
            let target = "List-Id: News <news.example.com>\r\n\r\n";
            let sibling = "List-Id: Other <other.example.com>\r\n\r\n";
            format!(
                "* 1 FETCH (UID 5 BODY[HEADER.FIELDS (LIST-ID)] {{{}}}\r\n{target})\r\n* 2 FETCH (UID 7 BODY[HEADER.FIELDS (LIST-ID)] {{{}}}\r\n{sibling})\r\n{tag} OK FETCH completed\r\n",
                target.len(),
                sibling.len()
            )
        })
        .await;

        let mk = Agentmail::new(Config::empty());
        let caps = imap_client::ServerCaps::from_strings(["MOVE".to_string()]);
        let totals = mk
            .matching_sweep_loop(
                &mut session,
                "test-account",
                &DeleteSelector::ListId("news.example.com".to_string()),
                &["INBOX".to_string()],
                SweepAction::Move {
                    destination: "Statements",
                },
                &caps,
                None,
                None,
            )
            .await
            .expect("scripted move sweep succeeds");

        assert_eq!(totals.found, 1);
        assert_eq!(totals.affected, 1, "the confirmed match is moved");
        assert_eq!(totals.failed, 0);

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        let joined = commands.concat();
        assert!(
            joined.contains("UID MOVE 5"),
            "moves via UID MOVE, exactly the confirmed UID: {commands:?}"
        );
        assert!(
            joined.contains("Statements"),
            "destination reaches the wire: {commands:?}"
        );
        assert!(
            !joined.contains("EXPUNGE") || !commands.iter().any(|c| c.contains("UID EXPUNGE")),
            "a move sweep must never expunge: {commands:?}"
        );
        assert!(
            !joined.contains("UID MOVE 5,7"),
            "the sibling list's UID must stay put: {commands:?}"
        );
    }

    /// End-to-end sender-fallback sweep with the pertinence constraint: both
    /// candidates are bulk mail from the exact sender, but only the one whose
    /// List-Id matches the constraint is deleted — the sibling list survives.
    #[tokio::test]
    async fn delete_sweep_sender_fallback_is_scoped_to_the_constrained_list_id() {
        let (mut session, server) = scripted_sweep_session(" 11 12", |tag| {
            let matching = "From: News <sender@example.com>\r\nList-Unsubscribe: <https://x.example/u>\r\nList-Id: News <news.example.com>\r\n\r\n";
            let sibling = "From: News <sender@example.com>\r\nList-Unsubscribe: <https://x.example/u>\r\nList-Id: Other <other.example.com>\r\n\r\n";
            format!(
                "* 1 FETCH (UID 11 BODY[HEADER] {{{}}}\r\n{matching})\r\n* 2 FETCH (UID 12 BODY[HEADER] {{{}}}\r\n{sibling})\r\n{tag} OK FETCH completed\r\n",
                matching.len(),
                sibling.len()
            )
        })
        .await;

        let mk = Agentmail::new(Config::empty());
        let caps = imap_client::ServerCaps::from_strings(["UIDPLUS".to_string()]);
        let totals = mk
            .matching_sweep_loop(
                &mut session,
                "test-account",
                &DeleteSelector::Sender {
                    email: "sender@example.com".to_string(),
                    name: "News".to_string(),
                    bulk_only: true,
                    list_id: Some("news.example.com".to_string()),
                },
                &["INBOX".to_string()],
                SweepAction::Delete {
                    trash: None,
                    allow_permanent_fallback: false,
                },
                &caps,
                None,
                None,
            )
            .await
            .expect("scripted sweep succeeds");

        assert_eq!(
            totals.found, 1,
            "only the constrained list's message qualifies"
        );
        assert_eq!(totals.affected, 1);
        assert_eq!(totals.failed, 0);

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        let joined = commands.concat();
        assert!(
            joined.contains("UID STORE 11 +FLAGS (\\Deleted)"),
            "deletes only the constrained match: {commands:?}"
        );
        assert!(
            !joined.contains("UID STORE 11,12"),
            "the sibling list's UID 12 must survive the sweep: {commands:?}"
        );
    }
}
