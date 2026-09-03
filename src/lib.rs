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
pub(crate) mod record;
pub mod scan_cache;
pub mod secret;
pub mod types;

mod authentication;
mod domain;
mod header_cache;
mod mailbox_catalog;
mod mutation_journal;
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

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

const MAX_DRAFT_RECIPIENTS: usize = 100;
const MAX_DRAFT_BODY_BYTES: usize = 1024 * 1024;
const MAX_DRAFT_ATTACHMENTS: usize = 20;
const MAX_DRAFT_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_DRAFT_ATTACHMENTS_TOTAL_BYTES: usize = 40 * 1024 * 1024;
const MAX_DRAFT_MIME_BYTES: usize = 64 * 1024 * 1024;
const MAX_PAGE_OFFSET: usize = 1_000_000;
const MAX_PAGE_LIMIT: usize = 100;
const MAX_MAILBOX_PAGE_LIMIT: usize = 500;
const MAX_SUBSCRIPTION_SAMPLE_HEADER_BYTES: usize = 256 * 1024;
const MAX_THREAD_RECORD_MESSAGES: usize = 100;
const MAX_THREAD_RECORD_HEADER_BYTES: usize = 256 * 1024;

/// High-level facade for IMAP operations.
/// Owns the connection pool and configuration.
pub struct Agentmail {
    pool: ConnectionPool,
    /// Per-account mailbox hierarchy used by completion and special-use lookup.
    mailbox_catalog: mailbox_catalog::MailboxCatalog,
    /// Persistent UID membership and immutable ranking-header projection.
    header_cache: header_cache::HeaderCache,
    /// Durable state machine for COPY-based MOVE recovery. This is separate
    /// from the disposable header cache by design.
    mutation_journal: mutation_journal::MutationJournal,
    /// Common per-account mutation boundary. Every public operation that can
    /// change server state takes this lock, so direct Rust calls and MCP task
    /// calls cannot race each other.
    mutation_locks: parking_lot::Mutex<
        std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>,
    >,
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
    uidonly: Option<bool>,
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

    /// Force born-UID-Mode on or off, overriding the default (on whenever the
    /// header cache is persistent). When on, every fresh connection for a
    /// UIDONLY-capable account (Yahoo/AOL) enters RFC 9586 UID Mode at connect,
    /// so a single held connection serves rankings, reads, and sweeps with no
    /// mid-life Limited↔UID switch — each switch is another LOGIN on
    /// rate-limited providers. Off keeps the classic two-pool behavior (UID
    /// Mode entered lazily per scan). Non-UIDONLY servers (Gmail/Outlook) never
    /// advertise the capability and are unaffected either way. Turning it on
    /// without a persistent cache makes each ranking re-walk the full mailbox
    /// (correct, but unamortized), so the default ties it to cache persistence.
    pub fn uidonly(mut self, enabled: bool) -> Self {
        self.uidonly = Some(enabled);
        self
    }

    /// Validate and normalize programmatic configuration before construction.
    /// New embedding code should prefer this over [`Self::build`].
    pub fn try_build(mut self) -> Result<Agentmail> {
        self.config.normalize_and_validate()?;
        Ok(self.finish())
    }

    /// Build without revalidating configuration. Retained for source
    /// compatibility; configs loaded from disk have already been validated.
    pub fn build(self) -> Agentmail {
        self.finish()
    }

    fn finish(self) -> Agentmail {
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
        let (header_cache, mutation_journal) = match self.cache {
            CacheLocation::Auto => (
                header_cache::HeaderCache::default(),
                mutation_journal::MutationJournal::default_persistent(),
            ),
            // Disabling the ranking cache must not silently disable mutation
            // durability. COPY-based MOVE still needs its independent journal.
            CacheLocation::Disabled => (
                header_cache::HeaderCache::disabled(),
                mutation_journal::MutationJournal::default_persistent(),
            ),
            CacheLocation::Dir(dir) => (
                header_cache::HeaderCache::at_path(dir.join(header_cache::HeaderCache::FILE_NAME)),
                mutation_journal::MutationJournal::at_path(
                    dir.join(mutation_journal::MutationJournal::FILE_NAME),
                ),
            ),
        };
        // Born-UID-Mode defaults to on exactly when the cache can amortize the
        // full-mailbox UID walk; disabling the cache keeps the windowed
        // Limited-Mode path unchanged. An explicit `.uidonly(..)` overrides.
        pool.set_uidonly(self.uidonly.unwrap_or_else(|| header_cache.is_persistent()));
        Agentmail {
            pool,
            mailbox_catalog: mailbox_catalog::MailboxCatalog::default(),
            header_cache,
            mutation_journal,
            mutation_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
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

    /// Validate and normalize a programmatically assembled configuration.
    pub fn try_new(config: Config) -> Result<Self> {
        Self::builder(config).try_build()
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
            uidonly: None,
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
        self.pool
            .account_config(account)
            .map(config::AccountConfig::canonical_addresses)
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    fn mutation_account_key(&self, account: &str) -> Result<String> {
        let config = self
            .pool
            .account_config(account)
            .ok_or_else(|| AgentmailError::AccountNotFound(account.to_string()))?;
        Ok(format!(
            "{}:{}|{}|{}:{}",
            config.host.to_ascii_lowercase(),
            config.port,
            u8::from(config.tls),
            config.username.len(),
            config.username
        ))
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

    fn validate_page_with_max(offset: usize, limit: usize, max_limit: usize) -> Result<()> {
        if offset > MAX_PAGE_OFFSET {
            return Err(AgentmailError::Other(format!(
                "offset must be at most {MAX_PAGE_OFFSET}"
            )));
        }
        if !(1..=max_limit).contains(&limit) {
            return Err(AgentmailError::Other(format!(
                "limit must be between 1 and {max_limit}"
            )));
        }
        offset.checked_add(limit).ok_or_else(|| {
            AgentmailError::Other("offset plus limit exceeds the supported range".to_string())
        })?;
        Ok(())
    }

    fn validate_page(offset: usize, limit: usize) -> Result<()> {
        Self::validate_page_with_max(offset, limit, MAX_PAGE_LIMIT)
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

    /// Serialize every server-side mutation for one account at the library
    /// boundary. Weak entries keep the map bounded after inactive accounts
    /// have no queued or running mutation.
    async fn lock_account_mutation(&self, account: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.mutation_locks.lock();
            if let Some(lock) = locks.get(account).and_then(std::sync::Weak::upgrade) {
                lock
            } else {
                if locks.len() >= 64 {
                    locks.retain(|_, lock| lock.strong_count() > 0);
                }
                let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(account.to_string(), std::sync::Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
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
        Self::validate_page_with_max(offset, limit, MAX_MAILBOX_PAGE_LIMIT)?;
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

    /// The mailbox's current UIDVALIDITY — the `{uidValidity}` epoch every
    /// `email://` resource URI carries.
    ///
    /// `STATUS (UIDVALIDITY …)` rather than SELECT/EXAMINE on purpose: it is a
    /// single round trip and does NOT change the connection's selected mailbox,
    /// so resolving it (for a completion, say) can never disturb a read that is
    /// already in flight on that session.
    pub(crate) async fn mailbox_uid_validity(&self, account: &str, mailbox: &str) -> Result<u32> {
        if !self.pool.config().accounts.contains_key(account) {
            return Err(AgentmailError::AccountNotFound(account.to_string()));
        }
        let mailbox_name = mailbox.to_string();
        let status = self
            .pool
            .with_session_retry(account, async move |session| {
                // `false`: we want UIDVALIDITY only, so there's no reason to ask
                // for HIGHESTMODSEQ and risk the CONDSTORE fallback round trip.
                imap_client::mailbox_status(session, &mailbox_name, false).await
            })
            .await?;
        imap_client::require_uid_validity(mailbox, status.uid_validity)
    }

    /// Create a new mailbox on the server.
    pub async fn create_mailbox(
        &self,
        account: &str,
        mailbox_name: &str,
    ) -> Result<CreateMailboxResponse> {
        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;

        // Check if mailbox already exists (make CREATE idempotent)
        let names = imap_client::list_mailbox_names(session.session()).await?;
        if names
            .iter()
            .any(|name| mailbox_names_equal(name, mailbox_name))
        {
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

    /// Preview or perform a guarded mailbox rename.
    #[allow(clippy::too_many_arguments)]
    pub async fn rename_mailbox(
        &self,
        account: &str,
        mailbox_name: &str,
        new_mailbox_name: &str,
        confirm_rename: bool,
        expected_message_count: Option<u32>,
        confirm_special_use: bool,
        confirm_descendants: bool,
    ) -> Result<RenameMailboxResponse> {
        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;
        let layout = imap_client::list_mailbox_layout(session.session()).await?;
        let entry = find_mailbox_layout(&layout, mailbox_name)
            .ok_or_else(|| AgentmailError::MailboxNotFound(mailbox_name.to_string()))?;
        if mailbox_names_equal(&entry.path, "INBOX") {
            return Err(AgentmailError::Other(
                "INBOX cannot be renamed through AgentMail".to_string(),
            ));
        }
        if find_mailbox_layout(&layout, new_mailbox_name).is_some() {
            return Err(AgentmailError::Other(format!(
                "destination mailbox '{new_mailbox_name}' already exists"
            )));
        }
        self.ensure_mailbox_not_in_pending_move(account, entry)
            .await?;
        let preflight = mailbox_mutation_preflight(
            session.session(),
            entry,
            &layout,
            MailboxMutationKind::Rename,
        )
        .await?;
        if !confirm_rename {
            session.release().await;
            return Ok(RenameMailboxResponse {
                account: account.to_string(),
                mailbox: entry.path.clone(),
                new_mailbox: new_mailbox_name.to_string(),
                preview: true,
                renamed: false,
                preflight,
            });
        }
        require_expected_message_count(expected_message_count, preflight.message_count)?;
        if !preflight.roles.is_empty() && !confirm_special_use {
            return Err(AgentmailError::Other(
                "mailbox has special-use roles; repeat with confirmSpecialUse=true".to_string(),
            ));
        }
        if !preflight.descendants.is_empty() && !confirm_descendants {
            return Err(AgentmailError::Other(
                "mailbox has descendants; repeat with confirmDescendants=true".to_string(),
            ));
        }

        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);
        let rename_result =
            imap_client::rename_mailbox(session.session(), &entry.path, new_mailbox_name).await;
        self.invalidate_mailbox_catalog(account);
        let renamed = match rename_result {
            Ok(()) => true,
            Err(error) if error.is_connection_error() => {
                drop(session);
                let (fresh_session, refreshed) = self
                    .mailbox_layout_after_ambiguous_mutation(account, "rename", &error)
                    .await?;
                session = fresh_session;
                let old_exists = find_mailbox_layout(&refreshed, &entry.path).is_some();
                let new_exists = find_mailbox_layout(&refreshed, new_mailbox_name).is_some();
                if !old_exists && new_exists {
                    true
                } else if old_exists && !new_exists {
                    return Err(error);
                } else {
                    return Err(AgentmailError::Other(format!(
                        "rename outcome is ambiguous after transport failure: oldExists={old_exists}, newExists={new_exists}; inspect list_mailboxes before retrying"
                    )));
                }
            }
            Err(error) => return Err(error),
        };
        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);
        session.release().await;
        Ok(RenameMailboxResponse {
            account: account.to_string(),
            mailbox: entry.path.clone(),
            new_mailbox: new_mailbox_name.to_string(),
            preview: false,
            renamed,
            preflight,
        })
    }

    /// Preview or perform a guarded mailbox delete.
    #[allow(clippy::too_many_arguments)]
    pub async fn delete_mailbox(
        &self,
        account: &str,
        mailbox_name: &str,
        confirm_delete: bool,
        expected_message_count: Option<u32>,
        confirm_non_empty: bool,
        confirm_special_use: bool,
        confirm_descendants: bool,
    ) -> Result<DeleteMailboxResponse> {
        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;
        let layout = imap_client::list_mailbox_layout(session.session()).await?;
        let Some(entry) = find_mailbox_layout(&layout, mailbox_name) else {
            session.release().await;
            self.invalidate_mailbox_catalog(account);
            return Ok(DeleteMailboxResponse {
                account: account.to_string(),
                mailbox: mailbox_name.to_string(),
                preview: false,
                deleted: false,
                already_missing: true,
                preflight: None,
            });
        };
        if mailbox_names_equal(&entry.path, "INBOX") {
            return Err(AgentmailError::Other(
                "INBOX cannot be deleted through AgentMail".to_string(),
            ));
        }
        self.ensure_mailbox_not_in_pending_move(account, entry)
            .await?;
        let preflight = mailbox_mutation_preflight(
            session.session(),
            entry,
            &layout,
            MailboxMutationKind::Delete,
        )
        .await?;
        if !confirm_delete {
            session.release().await;
            return Ok(DeleteMailboxResponse {
                account: account.to_string(),
                mailbox: entry.path.clone(),
                preview: true,
                deleted: false,
                already_missing: false,
                preflight: Some(preflight),
            });
        }
        require_expected_message_count(expected_message_count, preflight.message_count)?;
        if preflight.message_count > 0 && !confirm_non_empty {
            return Err(AgentmailError::Other(
                "mailbox is non-empty; repeat with confirmNonEmpty=true".to_string(),
            ));
        }
        if !preflight.roles.is_empty() && !confirm_special_use {
            return Err(AgentmailError::Other(
                "mailbox has special-use roles; repeat with confirmSpecialUse=true".to_string(),
            ));
        }
        if !preflight.descendants.is_empty() && !confirm_descendants {
            return Err(AgentmailError::Other(
                "mailbox has descendants; repeat with confirmDescendants=true".to_string(),
            ));
        }

        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);
        let delete_result = imap_client::delete_mailbox(session.session(), &entry.path).await;
        self.invalidate_mailbox_catalog(account);
        let deleted = match delete_result {
            Ok(()) => true,
            Err(error) if error.is_connection_error() => {
                drop(session);
                let (fresh_session, refreshed) = self
                    .mailbox_layout_after_ambiguous_mutation(account, "delete", &error)
                    .await?;
                session = fresh_session;
                if find_mailbox_layout(&refreshed, &entry.path).is_none() {
                    true
                } else {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        };
        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);
        session.release().await;
        Ok(DeleteMailboxResponse {
            account: account.to_string(),
            mailbox: entry.path.clone(),
            preview: false,
            deleted,
            already_missing: false,
            preflight: Some(preflight),
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
        Self::validate_page(offset, limit)?;
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
        Self::validate_page(offset, limit)?;
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
        // Reuse an idle UID-Mode session (skips both LOGIN and ENABLE UIDONLY).
        if self.header_cache.is_persistent()
            && let Some(caps) = self.pool.cached_caps(account)
            && caps.has("UIDONLY")
            && let Some(session) = self.pool.try_acquire_uid_mode(account).await?
        {
            return Ok((session, Some(Self::uid_page_size(&caps))));
        }
        let mut session = self.pool.acquire(account).await?;
        // Born-UID accounts come back already in UID Mode — no ENABLE, no
        // Limited↔UID switch (that switch is another LOGIN on Yahoo/AOL). A
        // UID-Mode connection always does the full MESSAGELIMIT walk, the
        // correct enumeration once UIDONLY is on.
        if session.is_uid_mode() {
            let page = self
                .pool
                .cached_caps(account)
                .map_or(imap_client::MAX_FETCH_CHUNK as u32, |caps| {
                    Self::uid_page_size(&caps)
                });
            return Ok((session, Some(page)));
        }
        let uid_mode = self.enter_uid_mode(account, session.session()).await?;
        if uid_mode.is_some() {
            session.mark_uid_mode();
        }
        Ok((session, uid_mode))
    }

    /// The per-command page size for a UID-Mode walk: the server's advertised
    /// `MESSAGELIMIT` (RFC 9738), or the default fetch chunk when unbounded.
    fn uid_page_size(caps: &imap_client::ServerCaps) -> u32 {
        caps.message_limit()
            .unwrap_or(imap_client::MAX_FETCH_CHUNK as u32)
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
    ) -> hashbrown::HashMap<(String, u32, u32), String> {
        let SampleSubjects { subjects, missing } = sample_subjects(session, samples, cancel).await;
        if !missing.is_empty() {
            tracing::debug!(
                target: "agentmail",
                pruned = missing.len(),
                "pruning ranking samples the server no longer has"
            );
            if let Some(config) = self.pool.account_config(account) {
                for (mailbox, uid_validity, uid) in &missing {
                    self.header_cache
                        .prune_uid(account, config, mailbox, *uid_validity, *uid)
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
        Self::validate_page(offset, limit)?;
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
            if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                continue; // already counted this message from another folder
            }
            if row.sender_email.is_empty() || own.contains(&row.sender_email) {
                continue;
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
        let total_messages = senders
            .iter()
            .fold(0_u32, |total, sender| total.saturating_add(sender.count));
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

    /// Group messages by the exact canonical domain of the first parsed
    /// Header From address. Parent domains and subdomains are separate rows;
    /// the Header From value is organizational metadata, not proof of DKIM
    /// ownership.
    pub async fn top_domains(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopDomainsResponse> {
        Self::validate_page(offset, limit)?;
        for attempt in 1..=Self::SCAN_RESUME_ATTEMPTS {
            match self
                .top_domains_once(mailbox, account, offset, limit, on_progress, cancel)
                .await
            {
                Err(error)
                    if error.is_connection_error() && attempt < Self::SCAN_RESUME_ATTEMPTS =>
                {
                    Self::scan_resume_backoff(attempt, "top_domains", cancel).await?;
                }
                other => return other,
            }
        }
        unreachable!("the final attempt always returns")
    }

    async fn top_domains_once(
        &self,
        mailbox: Option<&str>,
        account: &str,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<TopDomainsResponse> {
        let (mut session, uid_mode) = self.acquire_uid_scan(account).await?;
        let mailboxes = match mailbox {
            Some(mailbox) => vec![mailbox.to_string()],
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
        let own_vec = own.iter().cloned().collect::<Vec<_>>();

        if let Some(page) = self
            .header_cache
            .top_domains_page(
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
            let unique_domains = usize::try_from(page.total_groups).unwrap_or(usize::MAX);
            let total_messages = u32::try_from(page.total_messages).unwrap_or(u32::MAX);
            let cached_row_count = page.items.len();
            let cached_domains = page
                .items
                .into_iter()
                .map(|row| {
                    let identity = domain::domain_identity(&row.domain)?;
                    Some(DomainSummary {
                        domain: identity.domain,
                        registrable_domain: identity.registrable_domain,
                        subdomain: identity.subdomain,
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
                })
                .collect::<Option<Vec<_>>>();
            if let Some(mut domains) = cached_domains {
                let samples = domains
                    .iter()
                    .map(|row| row.sample.clone())
                    .collect::<Vec<_>>();
                let subjects = self
                    .page_sample_subjects(session.session(), account, &samples, cancel)
                    .await;
                for row in &mut domains {
                    row.subject = subjects
                        .get(&(
                            row.sample.mailbox.clone(),
                            row.sample.uid_validity,
                            row.sample.uid,
                        ))
                        .cloned();
                }
                Self::uid_mode_release(session, uid_mode).await;
                return Ok(TopDomainsResponse {
                    mailbox: mailbox.unwrap_or("*").to_string(),
                    account: account.to_string(),
                    total_messages,
                    unique_domains,
                    offset,
                    limit,
                    next_offset: next_offset(offset, domains.len(), unique_domains),
                    domains,
                });
            } else {
                tracing::warn!(
                    target: "agentmail",
                    cached_rows = cached_row_count,
                    "domain cache contained an invalid canonical domain; using live ranking"
                );
                // Keep the acquired session and continue through the live
                // path. Silently filtering a corrupt SQL row would make
                // OFFSET pagination repeat or skip otherwise valid groups.
            }
        }

        use hashbrown::{HashMap, HashSet};
        let mut grouped: HashMap<String, DomainSummary> = HashMap::new();
        let mut seen = HashSet::new();
        let rows = self
            .live_ranking_headers(session.session(), &mailboxes, on_progress, cancel)
            .await?;
        for (source_mailbox, row) in rows {
            if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                continue;
            }
            if row.sender_email.is_empty() || own.contains(&row.sender_email) {
                continue;
            }
            let Some(identity) = domain::domain_from_address(&row.sender_email)
                .and_then(|domain| domain::domain_identity(&domain))
            else {
                continue;
            };
            let uid_validity =
                row.uid_validity
                    .ok_or_else(|| AgentmailError::UidValidityUnavailable {
                        mailbox: source_mailbox.clone(),
                    })?;
            let entry = grouped
                .entry(identity.domain.clone())
                .or_insert_with(|| DomainSummary {
                    domain: identity.domain,
                    registrable_domain: identity.registrable_domain,
                    subdomain: identity.subdomain,
                    count: 0,
                    sample: MailboxMessageIdentity {
                        mailbox: source_mailbox.clone(),
                        uid_validity,
                        uid: row.uid,
                    },
                    subject: None,
                    oldest_date: None,
                    newest_date: None,
                });
            entry.count = entry.count.saturating_add(1);
            if ranking_sample_is_newer(
                (row.date, &source_mailbox, row.uid),
                (
                    entry.newest_date,
                    Some(entry.sample.mailbox.as_str()),
                    entry.sample.uid,
                ),
                entry.count == 1,
            ) {
                entry.sample = MailboxMessageIdentity {
                    mailbox: source_mailbox,
                    uid_validity,
                    uid: row.uid,
                };
            }
            if let Some(date) = row.date {
                entry.oldest_date = Some(entry.oldest_date.map_or(date, |oldest| oldest.min(date)));
                entry.newest_date = Some(entry.newest_date.map_or(date, |newest| newest.max(date)));
            }
        }

        let mut domains = grouped.into_values().collect::<Vec<_>>();
        domains.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.domain.cmp(&right.domain))
        });
        let unique_domains = domains.len();
        let total_messages = domains
            .iter()
            .fold(0_u32, |total, row| total.saturating_add(row.count));
        let mut domains = domains
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let samples = domains
            .iter()
            .map(|row| row.sample.clone())
            .collect::<Vec<_>>();
        let subjects = self
            .page_sample_subjects(session.session(), account, &samples, cancel)
            .await;
        for row in &mut domains {
            row.subject = subjects
                .get(&(
                    row.sample.mailbox.clone(),
                    row.sample.uid_validity,
                    row.sample.uid,
                ))
                .cloned();
        }
        imap_client::check_cancel(cancel)?;
        Self::uid_mode_release(session, uid_mode).await;
        Ok(TopDomainsResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_domains,
            offset,
            limit,
            next_offset: next_offset(offset, domains.len(), unique_domains),
            domains,
        })
    }

    /// Group mailing-list messages by normalized sender email.
    ///
    /// Includes messages that have List-Unsubscribe or List-Unsubscribe-Post.
    /// Display names and List-Id values do not split sender groups. The sample
    /// identity and one-click flag come from the newest message in each group.
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
        Self::validate_page(offset, limit)?;
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
                    address: row.address,
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
                    .get(&(
                        row.sample.mailbox.clone(),
                        row.sample.uid_validity,
                        row.sample.uid,
                    ))
                    .cloned();
            }
            Self::uid_mode_release(session, uid_mode).await;
            let unique_senders = usize::try_from(page.total_groups).unwrap_or(usize::MAX);
            return Ok(TopSubscriptionsResponse {
                mailbox: mailbox.unwrap_or("*").to_string(),
                account: account.to_string(),
                total_messages: u32::try_from(page.total_messages).unwrap_or(u32::MAX),
                unique_senders,
                offset,
                limit,
                next_offset: next_offset(offset, item_count, unique_senders),
                lists,
            });
        }

        use hashbrown::{HashMap, HashSet};
        use types::ListSummary;

        let mut map: HashMap<String, ListSummary> = HashMap::new();
        // Dedup the same logical message across folders (Gmail labels / All Mail).
        let mut seen: HashSet<String> = HashSet::new();
        // Don't rank the user themselves (their own sent mail).

        let live_rows = self
            .live_ranking_headers(session.session(), &mailboxes, on_progress, cancel)
            .await?;
        for (mbox, row) in live_rows {
            if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                continue; // already counted this message from another folder
            }
            if (row.list_unsubscribe.is_none() && row.list_unsubscribe_post.is_none())
                || row.sender_email.is_empty()
                || own.contains(&row.sender_email)
            {
                continue;
            }
            let key = row.sender_email.clone();
            let uid_validity =
                row.uid_validity
                    .ok_or_else(|| AgentmailError::UidValidityUnavailable {
                        mailbox: mbox.clone(),
                    })?;
            let entry = map.entry(key).or_insert_with(|| ListSummary {
                address: row.sender_email.clone(),
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
        });

        let unique_senders = lists.len();
        let total_messages = lists
            .iter()
            .fold(0_u32, |total, list| total.saturating_add(list.count));
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
                .get(&(
                    row.sample.mailbox.clone(),
                    row.sample.uid_validity,
                    row.sample.uid,
                ))
                .cloned();
        }
        session.release().await;

        Ok(TopSubscriptionsResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            total_messages,
            unique_senders,
            offset,
            limit,
            next_offset: next_offset(offset, item_count, unique_senders),
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
        Self::validate_page(offset, limit)?;
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
                    .get(&(
                        row.sample.mailbox.clone(),
                        row.sample.uid_validity,
                        row.sample.uid,
                    ))
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
            if !scan_cache::first_seen(&mut seen, row.message_id.as_deref()) {
                continue; // already counted this message from another folder
            }
            let raw_list_id = match row.list_id {
                Some(ref id) if !id.is_empty() => id.clone(),
                _ => continue, // Skip messages without List-Id
            };
            let Some(list_id) = normalize_list_id(&raw_list_id) else {
                continue;
            };
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
        let total_messages = lists
            .iter()
            .fold(0_u32, |total, list| total.saturating_add(list.count));
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
                .get(&(
                    row.sample.mailbox.clone(),
                    row.sample.uid_validity,
                    row.sample.uid,
                ))
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
            DeleteSelector::Sender { email, name } => {
                let candidates = candidate_sender_uids(session, email).await?;
                if candidates.is_empty() {
                    return Ok(Vec::new());
                }
                let fetched =
                    imap_client::fetch_senders_batch(session, &candidates, cancel).await?;
                Ok(fetched
                    .into_iter()
                    .filter(|(_uid, e, n)| e == email && n == name)
                    .map(|(uid, _, _)| uid)
                    .collect())
            }
            DeleteSelector::SubscriptionSender { email, list_id } => {
                let candidates = candidate_sender_uids(session, email).await?;
                if candidates.is_empty() {
                    return Ok(Vec::new());
                }
                filter_subscription_sender_mail(
                    session,
                    &candidates,
                    email,
                    list_id.as_deref(),
                    cancel,
                )
                .await
            }
            DeleteSelector::RankedSubscription { email, list_id } => {
                let candidates = candidate_sender_uids(session, email).await?;
                if candidates.is_empty() {
                    return Ok(Vec::new());
                }
                filter_ranked_subscription_mail(
                    session,
                    &candidates,
                    email,
                    list_id.as_deref(),
                    cancel,
                )
                .await
            }
            DeleteSelector::Domain(expected_domain) => {
                let expected_domain =
                    domain::canonicalize_domain(expected_domain).ok_or_else(|| {
                        AgentmailError::Other("domain must be a valid DNS domain name".to_string())
                    })?;
                let mut candidates = if expected_domain
                    .split('.')
                    .any(|label| label.starts_with("xn--"))
                {
                    // IMAP servers are not required to normalize an EAI
                    // U-label in From to the equivalent IDNA A-label used by
                    // the public domain identity. Enumerate the visible set
                    // for IDNs, then let the live parser below canonicalize
                    // exact matches. The outer drain/UID-Mode logic preserves
                    // whole-mailbox coverage on windowed providers.
                    let criteria = SearchCriteria {
                        deleted: Some(false),
                        ..Default::default()
                    };
                    let query = imap_client::build_search_query_pub(&criteria)?;
                    imap_client::search_uids(session, &query).await?
                } else {
                    let criteria = SearchCriteria {
                        // Candidate search only. The live From parse below is
                        // the mutation authority and enforces exact equality.
                        from: Some(format!("@{expected_domain}")),
                        deleted: Some(false),
                        ..Default::default()
                    };
                    let query = imap_client::build_search_query_pub(&criteria)?;
                    imap_client::search_uids(session, &query).await?
                };
                if let (Some(config), Some(uid_validity)) =
                    (self.pool.account_config(account), mb.uid_validity)
                {
                    candidates.extend(
                        self.header_cache
                            .cached_domain_uids(
                                account,
                                config,
                                mbox,
                                &expected_domain,
                                uid_validity,
                            )
                            .await,
                    );
                }
                candidates.sort_unstable();
                candidates.dedup();
                if candidates.is_empty() {
                    return Ok(Vec::new());
                }
                let own = self.own_addresses(account);
                let fetched =
                    imap_client::fetch_senders_batch(session, &candidates, cancel).await?;
                Ok(fetched
                    .into_iter()
                    .filter(|(_uid, email, _name)| {
                        !own.contains(email)
                            && domain::domain_from_address(email).as_deref()
                                == Some(expected_domain.as_str())
                    })
                    .map(|(uid, _, _)| uid)
                    .collect())
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
        match outcome {
            Ok(totals) if totals.session_usable => {
                // Release routes by the UID-Mode mark: UID store or Limited.
                Self::uid_mode_release(session, uid_mode).await;
                Ok(totals)
            }
            Ok(totals) => {
                // A timeout/EOF after mutation bytes makes the connection
                // unsafe to reuse even though the durable result is useful.
                drop(session);
                Ok(totals)
            }
            Err(error) => {
                // Conservative for every failed mutation path: a healthy
                // subsequent call can reconnect, a desynchronized one cannot.
                drop(session);
                Err(error)
            }
        }
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

        for (mailbox_index, mbox) in mailboxes.iter().enumerate() {
            let mut mailbox_found = 0usize;
            let mut mailbox_affected = 0usize;
            let mut mailbox_failed = 0usize;
            let mut mailbox_pending = 0usize;
            let mut mailbox_needs_attention = 0usize;
            let mut mailbox_operation_ids = Vec::new();
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
                let (affected, failed, pending, needs_attention, operation_ids, session_usable) =
                    match action {
                        SweepAction::Delete {
                            trash,
                            allow_permanent_fallback,
                        } => {
                            let account_key = if trash.is_some() && !caps.has_move() {
                                self.mutation_account_key(account)?
                            } else {
                                String::new()
                            };
                            let source_uid_validity = mb.uid_validity.ok_or_else(|| {
                                AgentmailError::UidValidityUnavailable {
                                    mailbox: mbox.clone(),
                                }
                            })?;
                            let result = imap_client::bulk_delete_messages_with_policy(
                                session,
                                &uids,
                                trash,
                                caps,
                                allow_permanent_fallback,
                                imap_client::JournalMoveContext {
                                    journal: &self.mutation_journal,
                                    account_key: &account_key,
                                    source_mailbox: mbox,
                                    source_uid_validity,
                                },
                                on_progress,
                                cancel,
                            )
                            .await?;
                            totals.trash_fallback |= result.trash_fallback;
                            (
                                result.deleted.len(),
                                result.failed.len(),
                                result.pending.len(),
                                result.needs_attention.len(),
                                result.operation_ids,
                                result.session_usable,
                            )
                        }
                        SweepAction::Move { destination } => {
                            let account_key = if caps.has_move() {
                                String::new()
                            } else {
                                self.mutation_account_key(account)?
                            };
                            let source_uid_validity = mb.uid_validity.ok_or_else(|| {
                                AgentmailError::UidValidityUnavailable {
                                    mailbox: mbox.clone(),
                                }
                            })?;
                            let result = imap_client::bulk_move_messages(
                                session,
                                &uids,
                                destination,
                                caps,
                                imap_client::JournalMoveContext {
                                    journal: &self.mutation_journal,
                                    account_key: &account_key,
                                    source_mailbox: mbox,
                                    source_uid_validity,
                                },
                                on_progress,
                                cancel,
                            )
                            .await?;
                            (
                                result.moved.len(),
                                result.failed.len(),
                                result.pending.len(),
                                result.needs_attention.len(),
                                result.operation_ids,
                                result.session_usable,
                            )
                        }
                    };
                totals.session_usable &= session_usable;
                if session_usable {
                    imap_client::sync(session).await?;
                }
                mailbox_found += uids.len();
                mailbox_affected += affected;
                mailbox_failed += failed;
                mailbox_pending += pending;
                mailbox_needs_attention += needs_attention;
                mailbox_operation_ids.extend(operation_ids);
                if affected == 0 || !session_usable {
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
            totals.pending += mailbox_pending;
            totals.needs_attention += mailbox_needs_attention;
            totals
                .operation_ids
                .extend(mailbox_operation_ids.iter().cloned());
            if mailbox_found > 0 {
                totals.mailboxes.push(SweepMailboxTally {
                    mailbox: mbox.clone(),
                    found: mailbox_found,
                    affected: mailbox_affected,
                    failed: mailbox_failed,
                    pending: mailbox_pending,
                    needs_attention: mailbox_needs_attention,
                    operation_ids: mailbox_operation_ids,
                });
            }
            if !totals.session_usable {
                // No later mailbox was attempted after an ambiguous mutation
                // invalidated this connection. Report that coverage gap
                // explicitly instead of returning an apparently complete
                // account-wide result.
                totals
                    .skipped
                    .extend(mailboxes.iter().skip(mailbox_index + 1).cloned());
                break;
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
        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await?;
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
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
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

    /// Add flags and/or set an Apple Mail color on one message.
    ///
    /// Thin wrapper over [`Self::update_flags`] — kept because it is public
    /// API and the CLI uses it. New callers that both add and remove should
    /// use `update_flags` directly and pay ONE UIDVALIDITY window.
    pub async fn add_flags(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        flags: &[String],
        color: Option<&str>,
    ) -> Result<UpdateFlagsResponse> {
        let color = color.map_or(FlagColorChange::Leave, |name| {
            FlagColorChange::Set(name.to_string())
        });
        self.update_flags(mailbox, account, uid, expected_uid_validity, flags, &[], color)
            .await
    }

    /// Remove flags and/or clear the Apple Mail color from one message.
    ///
    /// Thin wrapper over [`Self::update_flags`]; see the note there.
    pub async fn remove_flags(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        flags: &[String],
        remove_color: bool,
    ) -> Result<UpdateFlagsResponse> {
        let color = if remove_color {
            FlagColorChange::Clear
        } else {
            FlagColorChange::Leave
        };
        self.update_flags(mailbox, account, uid, expected_uid_validity, &[], flags, color)
            .await
    }

    /// Add and remove flags on one message in a SINGLE UIDVALIDITY window.
    ///
    /// Adding and removing used to be two tools and two library calls, so
    /// "mark read and clear the colour" meant two SELECTs, two epoch checks and
    /// a gap between them in which the mailbox could be renumbered — the second
    /// call then failed having already applied the first. One call, one window,
    /// one outcome.
    ///
    /// Order is REMOVE, then the colour change, then ADD. It is fixed and
    /// documented rather than incidental: a flag named in both lists ends up
    /// SET, and a colour survives a `remove` list that also names `\Flagged`.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_flags(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        add: &[String],
        remove: &[String],
        color: FlagColorChange,
    ) -> Result<UpdateFlagsResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        // Reject before the connection: an unknown colour is the caller's
        // mistake, and finding out after a partial STORE is worse than not
        // starting.
        let color_bits = match &color {
            FlagColorChange::Set(name) => Some(color_to_bits(name).ok_or_else(|| {
                AgentmailError::Other(format!(
                    "Unknown flag color '{name}'. Valid: red, orange, yellow, green, blue, purple, gray"
                ))
            })?),
            FlagColorChange::Leave | FlagColorChange::Clear => None,
        };

        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;
        imap_client::select_with_expected_uid_validity(
            session.session(),
            mailbox,
            expected_uid_validity,
        )
        .await?;

        const COLOR_BIT_KEYWORDS: [&str; 3] = ["$MailFlagBit0", "$MailFlagBit1", "$MailFlagBit2"];

        if !remove.is_empty() {
            imap_client::remove_flags(session.session(), uid, remove).await?;
        }

        match (&color, color_bits) {
            (FlagColorChange::Leave, _) => {}
            (FlagColorChange::Clear, _) => {
                let mut clear = vec!["\\Flagged".to_string()];
                clear.extend(COLOR_BIT_KEYWORDS.iter().map(|bit| (*bit).to_string()));
                imap_client::remove_flags(session.session(), uid, &clear).await?;
            }
            (FlagColorChange::Set(_), Some(bits)) => {
                // Clear the old bits first: the keywords are a 3-bit code, so
                // leaving a stale bit set would name a different colour.
                imap_client::remove_flags(
                    session.session(),
                    uid,
                    &COLOR_BIT_KEYWORDS
                        .iter()
                        .map(|bit| (*bit).to_string())
                        .collect::<Vec<_>>(),
                )
                .await?;
                let mut set = vec!["\\Flagged".to_string()];
                for (index, &bit) in COLOR_BIT_KEYWORDS.iter().enumerate() {
                    if bits[index] {
                        set.push(bit.to_string());
                    }
                }
                imap_client::add_flags(session.session(), uid, &set).await?;
            }
            (FlagColorChange::Set(_), None) => unreachable!("bits resolved above"),
        }

        if !add.is_empty() {
            imap_client::add_flags(session.session(), uid, add).await?;
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
        Self::validate_page(offset, limit)?;
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
        let _mutation_guard = self.lock_account_mutation(account).await;
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
            .await?;
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
        let account_key = if trash.is_some() && !caps.has_move() {
            self.mutation_account_key(account)?
        } else {
            String::new()
        };
        let result = imap_client::bulk_delete_messages_with_policy(
            session.session(),
            uids,
            trash.as_deref(),
            &caps,
            false,
            imap_client::JournalMoveContext {
                journal: &self.mutation_journal,
                account_key: &account_key,
                source_mailbox: mailbox,
                source_uid_validity: expected_uid_validity,
            },
            on_progress,
            cancel,
        )
        .await?;
        if result.session_usable {
            imap_client::sync(session.session()).await?;
            session.release().await;
        } else {
            drop(session);
        }

        Ok(DeleteMessagesResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            deleted: result.deleted.len(),
            failed: result.failed.len(),
            pending: result.pending.len(),
            needs_attention: result.needs_attention.len(),
            operation_ids: result.operation_ids,
            trash_fallback: result.trash_fallback,
            permanent: mode == DeleteMode::Permanent,
        })
    }

    /// Delete all messages from an exact sender identity (email + display
    /// name, as returned by a `top_senders` row).
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
        if email.is_empty() || email.contains(['<', '>', '\r', '\n', '\0']) {
            return Err(AgentmailError::Other(
                "sender email must be a bare address (use address from a top_senders row)"
                    .to_string(),
            ));
        }
        let email = parser::canonical_sender_address(email);
        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await?;
        if let Err(error) = Self::require_disposal_path(mode, trash.as_deref(), &caps) {
            session.release().await;
            return Err(error);
        }

        let sender_display = if name.is_empty() {
            email.clone()
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
                    email,
                    name: name.to_string(),
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
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
            mailboxes: delete_tallies(totals.mailboxes),
            skipped: totals.skipped,
            permanent: mode == DeleteMode::Permanent,
        })
    }

    /// Delete messages whose first parsed Header From address has one exact
    /// canonical domain. `example.com` never includes `mail.example.com`.
    pub async fn delete_by_domain(
        &self,
        mailbox: Option<&str>,
        account: &str,
        domain: &str,
        mode: DeleteMode,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<DeleteByDomainResponse> {
        let domain = domain::canonicalize_domain(domain).ok_or_else(|| {
            AgentmailError::Other("domain must be a valid DNS domain name".to_string())
        })?;
        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await?;
        if let Err(error) = Self::require_disposal_path(mode, trash.as_deref(), &caps) {
            session.release().await;
            return Err(error);
        }
        let mailboxes = match mailbox {
            Some(mailbox) => vec![mailbox.to_string()],
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
                DeleteSelector::Domain(domain.clone()),
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
        Ok(DeleteByDomainResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            domain,
            found: totals.found,
            deleted: totals.affected,
            failed: totals.failed,
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
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
            && mailbox_names_equal(mbox, destination)
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
            .any(|name| mailbox_names_equal(name, destination))
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
                .filter(|mbox| !mailbox_names_equal(mbox, destination))
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
        let _mutation_guard = self.lock_account_mutation(account).await;
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
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
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
        if email.is_empty() || email.contains(['<', '>', '\r', '\n', '\0']) {
            return Err(AgentmailError::Other(
                "sender email must be a bare address (use address from a top_senders row)"
                    .to_string(),
            ));
        }
        let email = parser::canonical_sender_address(email);
        let _mutation_guard = self.lock_account_mutation(account).await;
        let sender_display = if name.is_empty() {
            email.clone()
        } else {
            format!("{name} <{email}>")
        };
        let totals = self
            .move_sweep(
                mailbox,
                account,
                DeleteSelector::Sender {
                    email,
                    name: name.to_string(),
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
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
            mailboxes: move_tallies(totals.mailboxes),
            skipped: totals.skipped,
        })
    }

    /// Move messages from one exact canonical Header From domain. Subdomains
    /// remain independent rows/actions.
    pub async fn move_by_domain(
        &self,
        mailbox: Option<&str>,
        account: &str,
        domain: &str,
        destination: &str,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<MoveByDomainResponse> {
        let domain = domain::canonicalize_domain(domain).ok_or_else(|| {
            AgentmailError::Other("domain must be a valid DNS domain name".to_string())
        })?;
        let _mutation_guard = self.lock_account_mutation(account).await;
        let totals = self
            .move_sweep(
                mailbox,
                account,
                DeleteSelector::Domain(domain.clone()),
                destination,
                on_progress,
                cancel,
            )
            .await?;
        Ok(MoveByDomainResponse {
            mailbox: mailbox.unwrap_or("*").to_string(),
            account: account.to_string(),
            domain,
            destination: destination.trim().to_string(),
            found: totals.found,
            moved: totals.affected,
            failed: totals.failed,
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
            mailboxes: move_tallies(totals.mailboxes),
            skipped: totals.skipped,
        })
    }

    /// Move the exact bulk-mail subscription represented by one
    /// `top_subscriptions` sample.
    ///
    /// The sample UID is bound to its UIDVALIDITY epoch and re-fetched live.
    /// Matching then requires the exact canonical sender email, at least one
    /// list-action header, and the sample's normalized List-Id when it has one.
    /// The account-wide mutation plan is swept with the destination excluded.
    pub async fn move_subscription(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        destination: &str,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<MoveSubscriptionResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        if destination.trim().is_empty() {
            return Err(AgentmailError::Other("destination is required".to_string()));
        }
        imap_client::check_cancel(cancel)?;
        let _mutation_guard = self.lock_account_mutation(account).await;

        let mut session = self.pool.acquire(account).await?;
        let sample_headers = match imap_client::get_message_headers_bounded(
            session.session(),
            mailbox,
            uid,
            expected_uid_validity,
            MAX_SUBSCRIPTION_SAMPLE_HEADER_BYTES,
        )
        .await
        {
            Ok(headers) => headers,
            Err(error) => {
                if matches!(error, AgentmailError::MessageNotFound(_)) {
                    if let Some(config) = self.pool.account_config(account) {
                        self.header_cache
                            .prune_uid(account, config, mailbox, expected_uid_validity, uid)
                            .await;
                    }
                    session.release().await;
                }
                return Err(error);
            }
        };
        session.release().await;

        let (sender, _, _, _) = parser::parse_sender_date(&sample_headers).map_err(|error| {
            AgentmailError::Parse(format!(
                "top_subscriptions sample UID {uid} has no usable sender: {error}"
            ))
        })?;
        if sender.is_empty() {
            return Err(AgentmailError::Parse(format!(
                "top_subscriptions sample UID {uid} has no usable sender"
            )));
        }
        let headers = unsubscribe::parse_list_headers(&sample_headers);
        if headers.list_unsubscribe.is_none() && headers.list_unsubscribe_post.is_none() {
            return Err(AgentmailError::Other(format!(
                "message UID {uid} is no longer a top_subscriptions candidate; re-run top_subscriptions for a fresh sample"
            )));
        }
        let list_id = headers
            .has_single_list_id()
            .then_some(headers.list_id.as_deref())
            .flatten()
            .and_then(normalize_list_id);
        let matched_by = if list_id.is_some() {
            "exact sender email + list-action header + exact List-Id"
        } else {
            "exact sender email + list-action header"
        };

        let totals = self
            .move_sweep(
                None,
                account,
                DeleteSelector::RankedSubscription {
                    email: sender.clone(),
                    list_id: list_id.clone(),
                },
                destination,
                on_progress,
                cancel,
            )
            .await?;
        Ok(MoveSubscriptionResponse {
            mailbox: "*".to_string(),
            account: account.to_string(),
            sample_mailbox: mailbox.to_string(),
            sample_uid_validity: expected_uid_validity,
            sample_uid: uid,
            sender,
            list_id,
            matched_by: matched_by.to_string(),
            destination: destination.trim().to_string(),
            found: totals.found,
            moved: totals.affected,
            failed: totals.failed,
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
            mailboxes: move_tallies(totals.mailboxes),
            skipped: totals.skipped,
        })
    }

    /// List durable COPY-based MOVE operations that are awaiting cleanup or
    /// explicit review. Native UID MOVE never needs journal entries.
    pub async fn list_pending_moves(&self, account: &str) -> Result<ListPendingMovesResponse> {
        let account_key = self.mutation_account_key(account)?;
        let operations = self
            .mutation_journal
            .list_pending(&account_key)
            .await?
            .into_iter()
            .map(pending_move_from_operation)
            .collect();
        Ok(ListPendingMovesResponse {
            account: account.to_string(),
            operations,
        })
    }

    /// Reconcile all pending COPY-based moves for an account, or one durable
    /// operation ID. COPY is retried only when unchanged destination UIDNEXT
    /// proves the ambiguous command did not create a message.
    pub async fn reconcile_moves(
        &self,
        account: &str,
        operation_id: Option<&str>,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<ReconcileMovesResponse> {
        let _mutation_guard = self.lock_account_mutation(account).await;
        let account_key = self.mutation_account_key(account)?;
        let operations = if let Some(operation_id) = operation_id {
            let operation = self
                .mutation_journal
                .get(operation_id)
                .await?
                .ok_or_else(|| {
                    AgentmailError::Other(format!("unknown move operation '{operation_id}'"))
                })?;
            if operation.account_key != account_key {
                return Err(AgentmailError::Other(format!(
                    "move operation '{operation_id}' does not belong to account '{account}'"
                )));
            }
            vec![operation]
        } else {
            self.mutation_journal.list_pending(&account_key).await?
        };

        let total = operations.len() as u64;
        let mut completed = 0usize;
        let mut pending = 0usize;
        let mut needs_attention = 0usize;
        let mut failed = 0usize;
        for (index, operation) in operations.iter().cloned().enumerate() {
            imap_client::check_cancel(cancel)?;
            let mut session = self.pool.acquire(account).await?;
            let source_mailbox = operation.source_mailbox.clone();
            let outcome = imap_client::reconcile_journaled_move(
                session.session(),
                imap_client::JournalMoveContext {
                    journal: &self.mutation_journal,
                    account_key: &account_key,
                    source_mailbox: &source_mailbox,
                    source_uid_validity: operation.source_uid_validity,
                },
                operation,
            )
            .await;
            match outcome {
                Ok(outcome) => {
                    match outcome.status {
                        MoveStatus::Moved => completed += 1,
                        MoveStatus::Failed => failed += 1,
                        MoveStatus::ReconciliationPending => pending += 1,
                        MoveStatus::NeedsAttention => needs_attention += 1,
                    }
                    if outcome.session_usable {
                        let sync = imap_client::sync(session.session()).await;
                        if sync.is_ok() {
                            session.release().await;
                        } else {
                            drop(session);
                        }
                    } else {
                        drop(session);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "agentmail",
                        operation_id = operations[index].operation_id,
                        error = %error,
                        "move reconciliation attempt failed"
                    );
                    drop(session);
                    failed += 1;
                }
            }
            if let Some(progress) = on_progress {
                progress((index + 1) as u64, total);
            }
        }
        let operations = self
            .mutation_journal
            .list_pending(&account_key)
            .await?
            .into_iter()
            .map(pending_move_from_operation)
            .collect();
        Ok(ReconcileMovesResponse {
            account: account.to_string(),
            examined: usize::try_from(total).unwrap_or(usize::MAX),
            completed,
            pending,
            needs_attention,
            failed,
            operations,
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
        let _mutation_guard = self.lock_account_mutation(account).await;
        let mut session = self.pool.acquire(account).await?;

        // Validate destination mailbox exists
        let names = imap_client::list_mailbox_names(session.session()).await?;
        if !names
            .iter()
            .any(|name| mailbox_names_equal(name, destination))
        {
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
        let account_key = self.mutation_account_key(account)?;
        let outcome = imap_client::move_message(
            session.session(),
            uid,
            destination,
            &caps,
            imap_client::JournalMoveContext {
                journal: &self.mutation_journal,
                account_key: &account_key,
                source_mailbox: mailbox,
                source_uid_validity: expected_uid_validity,
            },
        )
        .await?;
        if outcome.session_usable {
            imap_client::sync(session.session()).await?;
            session.release().await;
        } else {
            drop(session);
        }

        Ok(MoveMessageResponse {
            mailbox: mailbox.to_string(),
            account: account.to_string(),
            uid,
            destination: destination.to_string(),
            moved: outcome.status == MoveStatus::Moved,
            status: outcome.status,
            operation_id: outcome.operation_id,
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
        self.create_draft_with_headers(
            account,
            subject,
            body,
            to,
            cc,
            bcc,
            &[],
            None,
            &[],
            attachments,
            None,
            draft::BodyFormat::default(),
        )
        .await
    }

    /// Create a draft with Reply-To and RFC threading headers.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_draft_with_headers(
        &self,
        account: &str,
        subject: &str,
        body: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
        reply_to: &[String],
        in_reply_to: Option<&str>,
        references: &[String],
        attachments: &[crate::types::DraftAttachment],
        warning: Option<String>,
        body_format: draft::BodyFormat,
    ) -> Result<CreateDraftResponse> {
        validate_draft_payload(body, to, cc, bcc, reply_to, attachments)?;

        let _mutation_guard = self.lock_account_mutation(account).await;

        let account_config = self
            .pool
            .account_config(account)
            .ok_or_else(|| AgentmailError::AccountNotFound(account.to_string()))?;
        let from = account_config
            .canonical_email()
            .unwrap_or_else(|| account_config.username.clone());
        let rfc822 = draft::compose_draft_with_headers(
            subject,
            body,
            to,
            cc,
            bcc,
            Some(&from),
            attachments,
            draft::DraftHeaderOptions {
                reply_to,
                in_reply_to,
                references,
                apple_uuid: uuid::Uuid::new_v4(),
                body_format,
            },
        )?;
        let mut session = self.pool.acquire(account).await?;
        // AFTER the connection, because the bound is the SERVER's — see
        // `check_draft_size`. `server_caps` is cached per account, so this is
        // not an extra round trip on a warm pool.
        let caps = self.pool.server_caps(account, session.session()).await?;
        if let Err(error) = check_draft_size(rfc822.len(), &caps) {
            session.release().await;
            return Err(error);
        }

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

        let identity = self
            .append_draft_with_recovery(account, &drafts_name, &rfc822, session)
            .await;
        self.invalidate_mailbox_catalog(account);
        let identity = identity?;

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
                reply_to: reply_to.to_vec(),
            },
            in_reply_to: in_reply_to.map(str::to_string),
            references: references.to_vec(),
            threading_applied: in_reply_to.is_some(),
            warning,
            attachments: attached_names,
            uid_validity: identity.map(|(uid_validity, _)| uid_validity),
            uid: identity.map(|(_, uid)| uid),
        })
    }

    /// Create a reply or reply-all draft from one live message identity.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_reply_draft(
        &self,
        account: &str,
        mailbox: &str,
        uid: u32,
        expected_uid_validity: u32,
        mode: ReplyMode,
        subject: Option<&str>,
        body: &str,
        bcc: &[String],
        reply_to: &[String],
        attachments: &[DraftAttachment],
        body_format: draft::BodyFormat,
    ) -> Result<CreateDraftResponse> {
        let response = self
            .get_messages_by_uid(
                mailbox,
                account,
                &[uid],
                expected_uid_validity,
                false,
                false,
            )
            .await?;
        let source = response
            .messages
            .into_iter()
            .next()
            .ok_or(AgentmailError::MessageNotFound(uid))?;
        let own = self.own_addresses(account);
        let reply_target = if source.reply_to.trim().is_empty() {
            source.sender.clone()
        } else {
            source.reply_to.clone()
        };
        let mut seen = hashbrown::HashSet::new();
        let mut to = Vec::new();
        push_reply_recipient(&mut to, &mut seen, &own, &reply_target);
        let mut cc = Vec::new();
        if mode == ReplyMode::ReplyAll {
            for recipient in &source.to {
                push_reply_recipient(&mut to, &mut seen, &own, recipient);
            }
            for recipient in &source.cc {
                push_reply_recipient(&mut cc, &mut seen, &own, recipient);
            }
        }
        if to.is_empty() && mode == ReplyMode::Reply {
            for recipient in source.to.iter().chain(&source.cc) {
                push_reply_recipient(&mut to, &mut seen, &own, recipient);
                if !to.is_empty() {
                    break;
                }
            }
        }
        if to.is_empty() && cc.is_empty() && bcc.is_empty() {
            return Err(AgentmailError::Other(
                "the source message has no reply recipient outside this account".to_string(),
            ));
        }

        let subject = subject
            .map(str::trim)
            .filter(|subject| !subject.is_empty())
            .map_or_else(|| reply_subject(&source.subject), str::to_string);
        let mut references = source.references;
        let (in_reply_to, warning) = match source.message_id {
            Some(message_id) => {
                if !references.iter().any(|reference| reference == &message_id) {
                    references.push(message_id.clone());
                }
                (Some(message_id), None)
            }
            None => (
                None,
                Some(
                    "source message has no Message-ID; recipients and subject were prepared, but RFC thread headers could not be applied"
                        .to_string(),
                ),
            ),
        };
        self.create_draft_with_headers(
            account,
            &subject,
            body,
            &to,
            &cc,
            bcc,
            reply_to,
            in_reply_to.as_deref(),
            &references,
            attachments,
            warning,
            body_format,
        )
        .await
    }

    /// Replace one live `\Draft`.
    ///
    /// Uses RFC 8508 UID REPLACE when the server advertises it — one atomic
    /// command, no window in which both drafts exist. Most servers do NOT:
    /// neither Iyahoo/iCloud nor Gmail offers REPLACE, so the atomic path is
    /// the exception rather than the rule. There we emulate it as
    /// APPEND-then-discard, in that order.
    ///
    /// The order is the whole safety argument. APPEND first means the new
    /// content is durable before anything is destroyed, so the worst failure
    /// is a DUPLICATE draft, never a lost one — and a duplicate is reported in
    /// `warning`, not raised as an error, because the caller's draft was in
    /// fact written and the new UID is the answer they asked for. Refusing the
    /// emulation outright (the pre-2026-09-03 behavior) did not avoid that
    /// risk; it exported it. Agents simply ran `create_draft` + `delete_messages`
    /// by hand, which is the same two commands with none of the guards below —
    /// no `\Draft` verification, no UIDVALIDITY fence, no policy-aware discard.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_draft(
        &self,
        account: &str,
        drafts_mailbox: &str,
        uid: u32,
        expected_uid_validity: u32,
        subject: &str,
        body: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
        reply_to: &[String],
        in_reply_to: Option<&str>,
        references: &[String],
        attachments: &[DraftAttachment],
        body_format: draft::BodyFormat,
    ) -> Result<UpdateDraftResponse> {
        validate_draft_payload(body, to, cc, bcc, reply_to, attachments)?;
        let _mutation_guard = self.lock_account_mutation(account).await;
        let account_config = self
            .pool
            .account_config(account)
            .ok_or_else(|| AgentmailError::AccountNotFound(account.to_string()))?;
        let from = account_config
            .canonical_email()
            .unwrap_or_else(|| account_config.username.clone());
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        imap_client::examine_with_expected_uid_validity(
            session.session(),
            drafts_mailbox,
            expected_uid_validity,
        )
        .await?;
        let current = imap_client::fetch_by_uids(
            session.session(),
            &[uid],
            drafts_mailbox,
            account,
            false,
            false,
        )
        .await?
        .into_iter()
        .next()
        .ok_or(AgentmailError::MessageNotFound(uid))?;
        if !current
            .flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Draft"))
        {
            return Err(AgentmailError::Other(format!(
                "mailbox '{drafts_mailbox}' UID {uid} is not marked \\Draft; refusing replacement"
            )));
        }
        let current_source = imap_client::get_message_source_bounded(
            session.session(),
            drafts_mailbox,
            uid,
            expected_uid_validity,
            MAX_DRAFT_MIME_BYTES,
        )
        .await?;
        let apple_uuid =
            draft::extract_apple_uuid(&current_source).unwrap_or_else(uuid::Uuid::new_v4);
        let replacement = draft::compose_draft_with_headers(
            subject,
            body,
            to,
            cc,
            bcc,
            Some(&from),
            attachments,
            draft::DraftHeaderOptions {
                reply_to,
                in_reply_to,
                references,
                apple_uuid,
                body_format,
            },
        )?;
        check_draft_size(replacement.len(), &caps)?;

        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);
        let (identity, warning) = if caps.has("REPLACE") {
            let identity = match imap_client::replace_draft(
                session.session(),
                drafts_mailbox,
                uid,
                expected_uid_validity,
                &replacement,
            )
            .await
            {
                Ok(identity) => identity,
                Err(error) if error.is_connection_error() => {
                    return Err(AgentmailError::Other(format!(
                        "draft replacement outcome is ambiguous after the IMAP connection failed; inspect the Drafts mailbox before retrying: {error}"
                    )));
                }
                Err(error) => return Err(error),
            };
            let identity = match (identity, draft::extract_message_id(&replacement)) {
                (Some(identity), _) => Some(identity),
                (None, Some(message_id)) => imap_client::find_uid_by_message_id(
                    session.session(),
                    drafts_mailbox,
                    &message_id,
                )
                .await
                .ok()
                .flatten(),
                (None, None) => None,
            };
            self.fence_header_cache_mutation(account).await;
            self.invalidate_mailbox_catalog(account);
            session.release().await;
            (identity, None)
        } else {
            self.emulate_replace_draft(
                account,
                drafts_mailbox,
                uid,
                expected_uid_validity,
                &replacement,
                session,
            )
            .await?
        };
        Ok(UpdateDraftResponse {
            updated: true,
            account: account.to_string(),
            drafts_mailbox: drafts_mailbox.to_string(),
            previous_uid_validity: expected_uid_validity,
            previous_uid: uid,
            uid_validity: identity.map(|(uid_validity, _)| uid_validity),
            uid: identity.map(|(_, uid)| uid),
            warning,
        })
    }

    /// RFC 8508 UID REPLACE emulated as APPEND-then-discard, for the majority
    /// of servers that do not implement it.
    ///
    /// Consumes `session`: `append_draft_with_recovery` owns the connection
    /// through its ambiguous-APPEND recovery (which may have to acquire a
    /// FRESH one), so the discard runs on a newly acquired session. Our
    /// caller holds the account mutation lock across both halves, so no other
    /// mutation can interleave between them.
    ///
    /// Returns the new draft's identity plus, only when the superseded draft
    /// survived, the warning describing it. A failed discard is NOT an error:
    /// the replacement is already written, and reporting failure would send
    /// the caller back to rewrite a draft that already exists.
    async fn emulate_replace_draft(
        &self,
        account: &str,
        drafts_mailbox: &str,
        superseded_uid: u32,
        expected_uid_validity: u32,
        replacement: &[u8],
        session: connection::PooledSession,
    ) -> Result<(Option<(u32, u32)>, Option<String>)> {
        // APPEND first — until this returns Ok, nothing has been destroyed.
        // A failure here leaves the original draft untouched and propagates.
        let identity = self
            .append_draft_with_recovery(account, drafts_mailbox, replacement, session)
            .await?;
        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);

        let warning = match self
            .discard_superseded_draft(
                account,
                drafts_mailbox,
                superseded_uid,
                expected_uid_validity,
            )
            .await
        {
            Ok(()) => None,
            Err(error) => {
                tracing::warn!(
                    target: "agentmail",
                    account,
                    mailbox = drafts_mailbox,
                    uid = superseded_uid,
                    error = %error,
                    "replacement draft was saved, but the superseded draft could not be discarded"
                );
                Some(format!(
                    "the replacement draft was saved, but the superseded draft (UID \
                     {superseded_uid}) could not be discarded and is still in \
                     '{drafts_mailbox}': {error}"
                ))
            }
        };
        self.fence_header_cache_mutation(account).await;
        self.invalidate_mailbox_catalog(account);
        Ok((identity, warning))
    }

    /// Delete exactly the superseded draft, through the SAME policy-aware path
    /// `delete_messages` uses — so Gmail routes to `[Gmail]/Trash` (an in-place
    /// EXPUNGE there only drops a label, leaving the draft alive in All Mail),
    /// and a server without UIDPLUS never reaches a plain EXPUNGE that would
    /// purge unrelated `\Deleted` messages.
    ///
    /// `Permanent` mirrors REPLACE, which expunges the message it supersedes —
    /// chosen only where the policy can honor it. Everywhere else the account's
    /// Trash is the disposal path, which is recoverable and needs only MOVE.
    async fn discard_superseded_draft(
        &self,
        account: &str,
        drafts_mailbox: &str,
        uid: u32,
        expected_uid_validity: u32,
    ) -> Result<()> {
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let mode = discard_mode_for(&caps);
        imap_client::select_with_expected_uid_validity(
            session.session(),
            drafts_mailbox,
            expected_uid_validity,
        )
        .await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await?;
        if let Err(error) = Self::require_disposal_path(mode, trash.as_deref(), &caps) {
            session.release().await;
            return Err(error);
        }
        // `trash_for_mode` may LIST for the special-use catalog, which leaves
        // no mailbox selected — re-select before the mutation, exactly as
        // `delete_messages` does.
        imap_client::select_with_expected_uid_validity(
            session.session(),
            drafts_mailbox,
            expected_uid_validity,
        )
        .await?;
        let account_key = if trash.is_some() && !caps.has_move() {
            self.mutation_account_key(account)?
        } else {
            String::new()
        };
        let result = imap_client::bulk_delete_messages_with_policy(
            session.session(),
            &[uid],
            trash.as_deref(),
            &caps,
            false,
            imap_client::JournalMoveContext {
                journal: &self.mutation_journal,
                account_key: &account_key,
                source_mailbox: drafts_mailbox,
                source_uid_validity: expected_uid_validity,
            },
            None,
            None,
        )
        .await?;
        if result.session_usable {
            imap_client::sync(session.session()).await?;
            session.release().await;
        } else {
            drop(session);
        }
        if result.deleted.contains(&uid) {
            Ok(())
        } else {
            Err(AgentmailError::Other(format!(
                "the discard reported no deletion (failed: {}, pending: {}, needs attention: {})",
                result.failed.len(),
                result.pending.len(),
                result.needs_attention.len()
            )))
        }
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

    /// Fetch one exact RFC822 message with `BODY.PEEK[]`, locally verify its
    /// DKIM signatures, and save the original bytes without overwriting.
    ///
    /// Filesystem confinement belongs to the caller: the MCP layer resolves
    /// `output_dir` through its internal `mcp::file_access` policy before calling this
    /// method. `filename` must nevertheless be one plain path component so a
    /// non-MCP caller cannot accidentally escape its chosen directory.
    pub async fn download_message_source(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        output_dir: &std::path::Path,
        filename: &str,
        cancel: Option<&CancelFn>,
    ) -> Result<DownloadedMessageSource> {
        validate_plain_filename(filename)?;
        imap_client::check_cancel(cancel)?;
        let raw = self
            .get_message_source_bytes_with_limit(
                mailbox,
                account,
                uid,
                expected_uid_validity,
                imap_client::MAX_TRANSIENT_MESSAGE_BYTES as u32,
            )
            .await?;
        imap_client::check_cancel(cancel)?;

        let mailbox_for_parse = mailbox.to_string();
        let account_for_parse = account.to_string();
        let (raw, sha256, metadata) = tokio::task::spawn_blocking(move || {
            use sha2::{Digest as _, Sha256};

            // sha2 0.11 digests are `hybrid_array::Array`, which dropped the
            // `LowerHex` impl GenericArray had — hex-encode byte-wise (same
            // pattern as `runtime_bootstrap::sha256_bytes`).
            let sha256 = Sha256::digest(&raw)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let metadata = parser::parse_rfc822(
                &raw,
                uid,
                Vec::new(),
                u32::try_from(raw.len()).ok(),
                &mailbox_for_parse,
                &account_for_parse,
                false,
                false,
            )
            .ok();
            (raw, sha256, metadata)
        })
        .await
        .map_err(|error| {
            AgentmailError::Other(format!("message archive analysis task failed: {error}"))
        })?;

        let dkim = authentication::verify_dkim(&raw, cancel).await?;
        imap_client::check_cancel(cancel)?;
        let path = output_dir.join(filename);
        write_new_private_file(&path, &raw).await?;
        let path = tokio::fs::canonicalize(&path).await.map_err(|error| {
            AgentmailError::Other(format!(
                "saved message source but could not resolve '{}': {error}",
                path.display()
            ))
        })?;

        let message_id = metadata
            .as_ref()
            .and_then(|message| message.message_id.clone());
        let date = metadata.as_ref().and_then(|message| message.date);
        let from_header = metadata
            .as_ref()
            .map(|message| message.sender.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let subject = metadata
            .as_ref()
            .map(|message| message.subject.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        Ok(DownloadedMessageSource {
            account: account.to_string(),
            mailbox: mailbox.to_string(),
            uid_validity: expected_uid_validity,
            uid,
            path: path.display().to_string(),
            bytes: raw.len(),
            sha256,
            message_id,
            date,
            from_header,
            subject,
            downloaded_at: chrono::Utc::now(),
            dkim,
        })
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

    /// Discover the bounded, exact RFC Message-ID graph around one live
    /// message. Subject similarity is intentionally never used: every
    /// selected identity must match `Message-ID`, `In-Reply-To`, or one token
    /// in `References` exactly after harmless angle-bracket normalization.
    pub async fn preview_thread_record(
        &self,
        mailbox: &str,
        account: &str,
        uid: u32,
        expected_uid_validity: u32,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> Result<ThreadRecordPreviewResponse> {
        Self::validate_uid_selector(mailbox, expected_uid_validity, &[uid])?;
        let mut session = self.pool.acquire(account).await?;
        imap_client::check_cancel(cancel)?;
        let seed_headers = imap_client::get_message_headers_bounded(
            session.session(),
            mailbox,
            uid,
            expected_uid_validity,
            MAX_THREAD_RECORD_HEADER_BYTES,
        )
        .await?;
        let seed_info = parser::parse_rfc822(
            &seed_headers,
            uid,
            Vec::new(),
            u32::try_from(seed_headers.len()).ok(),
            mailbox,
            account,
            false,
            false,
        )?;
        let seed_identity = MailboxMessageIdentity {
            mailbox: mailbox.to_string(),
            uid_validity: expected_uid_validity,
            uid,
        };
        let seed = thread_record_message(
            seed_identity.clone(),
            seed_info,
            vec!["seed identity supplied by the caller".to_string()],
        );

        let mailboxes = self
            .account_scan_mailboxes(
                account,
                session.session(),
                scan_plan::ScanPurpose::Discovery,
            )
            .await?;
        let mut messages = vec![seed.clone()];
        let mut seen_messages: hashbrown::HashSet<(String, u32, u32)> = hashbrown::HashSet::new();
        seen_messages.insert(thread_identity_key(&seed.identity));
        let mut known_ids = hashbrown::HashSet::new();
        let mut pending_ids = std::collections::VecDeque::new();
        queue_thread_ids(&seed, &mut known_ids, &mut pending_ids);
        let mut warnings = Vec::new();
        let mut truncated = false;

        'graph: while let Some(query_id) = pending_ids.pop_front() {
            imap_client::check_cancel(cancel)?;
            for candidate_mailbox in &mailboxes {
                imap_client::check_cancel(cancel)?;
                let selected = match imap_client::examine(session.session(), candidate_mailbox)
                    .await
                {
                    Ok(selected) => selected,
                    Err(error) => {
                        push_record_warning(
                            &mut warnings,
                            format!(
                                "skipped mailbox '{candidate_mailbox}' during thread discovery: {error}"
                            ),
                        );
                        continue;
                    }
                };
                let candidate_uid_validity = match imap_client::require_uid_validity(
                    candidate_mailbox,
                    selected.uid_validity,
                ) {
                    Ok(uid_validity) => uid_validity,
                    Err(error) => {
                        push_record_warning(
                            &mut warnings,
                            format!(
                                "skipped mailbox '{candidate_mailbox}' during thread discovery: {error}"
                            ),
                        );
                        continue;
                    }
                };
                let mut candidate_uids = hashbrown::HashSet::new();
                let mut search_failed = false;
                for header in ["Message-ID", "In-Reply-To", "References"] {
                    match imap_client::search_by_header(
                        session.session(),
                        header,
                        thread_search_value(&query_id),
                    )
                    .await
                    {
                        Ok(uids) => candidate_uids.extend(uids),
                        Err(error) => {
                            push_record_warning(
                                &mut warnings,
                                format!(
                                    "skipped mailbox '{candidate_mailbox}' query for {header}: {error}"
                                ),
                            );
                            search_failed = true;
                            break;
                        }
                    }
                }
                if search_failed {
                    continue;
                }

                let mut candidate_uids = candidate_uids.into_iter().collect::<Vec<_>>();
                candidate_uids.sort_unstable();
                for candidate_uid in candidate_uids {
                    let identity = MailboxMessageIdentity {
                        mailbox: candidate_mailbox.clone(),
                        uid_validity: candidate_uid_validity,
                        uid: candidate_uid,
                    };
                    if seen_messages.contains(&thread_identity_key(&identity)) {
                        continue;
                    }
                    let headers = match imap_client::get_message_headers_bounded(
                        session.session(),
                        candidate_mailbox,
                        candidate_uid,
                        candidate_uid_validity,
                        MAX_THREAD_RECORD_HEADER_BYTES,
                    )
                    .await
                    {
                        Ok(headers) => headers,
                        Err(error) => {
                            push_record_warning(
                                &mut warnings,
                                format!(
                                    "skipped {candidate_mailbox} UID {candidate_uid} during exact-header confirmation: {error}"
                                ),
                            );
                            continue;
                        }
                    };
                    let info = match parser::parse_rfc822(
                        &headers,
                        candidate_uid,
                        Vec::new(),
                        u32::try_from(headers.len()).ok(),
                        candidate_mailbox,
                        account,
                        false,
                        false,
                    ) {
                        Ok(info) => info,
                        Err(error) => {
                            push_record_warning(
                                &mut warnings,
                                format!(
                                    "skipped {candidate_mailbox} UID {candidate_uid} because its headers could not be parsed: {error}"
                                ),
                            );
                            continue;
                        }
                    };
                    let basis = thread_match_basis(&info, &query_id);
                    if basis.is_empty() {
                        continue;
                    }
                    if messages.len() == MAX_THREAD_RECORD_MESSAGES {
                        truncated = true;
                        break 'graph;
                    }
                    let message = thread_record_message(identity.clone(), info, basis);
                    seen_messages.insert(thread_identity_key(&identity));
                    queue_thread_ids(&message, &mut known_ids, &mut pending_ids);
                    messages.push(message);
                    if let Some(progress) = on_progress {
                        progress(messages.len() as u64, MAX_THREAD_RECORD_MESSAGES as u64);
                    }
                }
            }
        }
        session.release().await;

        messages.sort_by(|left, right| {
            left.date
                .cmp(&right.date)
                .then_with(|| left.identity.mailbox.cmp(&right.identity.mailbox))
                .then_with(|| left.identity.uid.cmp(&right.identity.uid))
        });
        if known_ids.is_empty() {
            warnings.push(
                "the seed has no Message-ID, In-Reply-To, or References values; the exact graph contains only the seed identity"
                    .to_string(),
            );
        }
        if truncated {
            warnings.push(format!(
                "the exact header graph exceeded {MAX_THREAD_RECORD_MESSAGES} storage identities; export is blocked until the selection is narrowed"
            ));
        }
        let selection_digest = thread_selection_digest(account, &seed_identity, &messages)?;
        Ok(ThreadRecordPreviewResponse {
            account: account.to_string(),
            seed: seed_identity,
            strategy: "exact-rfc-message-id-graph".to_string(),
            rationale: "Selected only live storage identities connected by exact Message-ID, In-Reply-To, or References values; subject similarity was not used."
                .to_string(),
            messages,
            selection_digest,
            confirmation_required: true,
            truncated,
            warnings,
        })
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
        #[cfg(unix)]
        let output_dir_existed = output_dir.exists();
        tokio::fs::create_dir_all(&output_dir).await.map_err(|e| {
            AgentmailError::Other(format!(
                "Failed to create directory '{}': {}",
                output_dir.display(),
                e
            ))
        })?;
        #[cfg(unix)]
        if !output_dir_existed {
            tokio::fs::set_permissions(
                &output_dir,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .await
            .map_err(|error| {
                AgentmailError::Other(format!(
                    "Failed to make download directory '{}' private: {error}",
                    output_dir.display()
                ))
            })?;
        }

        let mut downloaded = Vec::new();
        for (index, (name, content_type, bytes)) in attachments.iter().enumerate() {
            let filename = format!("{}_{}_{}", uid, index, sanitize_filename(name));
            let path = output_dir.join(&filename);
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                options.mode(0o600);
            }
            let mut file = options.open(&path).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    AgentmailError::Other(format!(
                        "refusing to overwrite existing attachment '{}'",
                        path.display()
                    ))
                } else {
                    AgentmailError::Other(format!("Failed to create '{}': {error}", path.display()))
                }
            })?;
            file.write_all(bytes).await.map_err(|error| {
                AgentmailError::Other(format!("Failed to write '{}': {error}", path.display()))
            })?;
            file.flush().await.map_err(|error| {
                AgentmailError::Other(format!("Failed to flush '{}': {error}", path.display()))
            })?;

            downloaded.push(DownloadedFile {
                index,
                path: filename.clone(),
                filename,
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
        let _mutation_guard = self.lock_account_mutation(account).await;

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
                            .prune_uid(account, config, mailbox, options.expected_uid_validity, uid)
                            .await;
                    }
                    session.release().await;
                }
                return Err(error);
            }
        };
        let headers = unsubscribe::parse_list_headers(&target.raw_message);
        let (target_email, _, _, _) =
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
                    "Matching-message cleanup was skipped: the sender's DKIM signature does not cover the List-Id header (List-Id is spoofable, so an unauthenticated value must not select an account-wide delete), and cleanup.identity was \"listIdOnly\". To clean up: use cleanup.identity \"listIdOrSender\" to match exact sender email + List-Unsubscribe-Post + this List-Id, use delete_list_id with an explicitly chosen listId, or use delete_by_sender."
                        .to_string(),
                );
                return Ok(response);
            }
            Err(CleanupIdentityError::NoUsableListId) => {
                response.cleanup_skipped_reason = Some(
                    "Matching-message cleanup was skipped: the message carries no single usable List-Id, and cleanup.identity was \"listIdOnly\". To clean up: use cleanup.identity \"listIdOrSender\" to match the exact sender email plus List-Unsubscribe-Post, or use delete_by_sender."
                        .to_string(),
                );
                return Ok(response);
            }
        };

        let mode = cleanup.mode();
        let mut session = self.pool.acquire(account).await?;
        let caps = self.pool.server_caps(account, session.session()).await?;
        let trash = self
            .trash_for_mode(mode, account, session.session(), &caps)
            .await?;
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
            CleanupIdentity::Sender { list_id } => DeleteSelector::SubscriptionSender {
                email: target_email.clone(),
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
            } => ("sender-email-list-id-fallback", Some(normalized)),
            CleanupIdentity::Sender { list_id: None } => ("sender-email-fallback", None),
        };
        let complete = totals.skipped.is_empty()
            && totals.failed == 0
            && totals.pending == 0
            && totals.needs_attention == 0;
        response.matching_messages = Some(MatchingMessagesResult {
            matched_by: matched_by.to_string(),
            sender: target_email,
            list_id,
            found: totals.found,
            deleted: totals.affected,
            failed: totals.failed,
            pending: totals.pending,
            needs_attention: totals.needs_attention,
            operation_ids: totals.operation_ids,
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

    async fn ensure_mailbox_not_in_pending_move(
        &self,
        account: &str,
        mailbox: &imap_client::MailboxLayout,
    ) -> Result<()> {
        if !self.mutation_journal.is_persistent() {
            return Ok(());
        }
        let account_key = self.mutation_account_key(account)?;
        let pending = self.mutation_journal.list_pending(&account_key).await?;
        let delimiter = mailbox.delimiter.as_deref();
        if let Some(operation) = pending.into_iter().find(|operation| {
            mailbox_is_same_or_descendant(&operation.source_mailbox, &mailbox.path, delimiter)
                || mailbox_is_same_or_descendant(&operation.destination, &mailbox.path, delimiter)
        }) {
            return Err(AgentmailError::Other(format!(
                "mailbox '{}' is referenced by pending move {}; reconcile that operation before renaming or deleting the mailbox",
                mailbox.path, operation.operation_id
            )));
        }
        Ok(())
    }

    /// Reconnect after a mailbox command lost its tagged completion. The old
    /// connection is never reused: after EOF or timeout its parser may still
    /// be waiting for bytes from the abandoned command.
    async fn mailbox_layout_after_ambiguous_mutation(
        &self,
        account: &str,
        operation: &str,
        original_error: &AgentmailError,
    ) -> Result<(connection::PooledSession, Vec<imap_client::MailboxLayout>)> {
        let mut session = self.pool.acquire(account).await.map_err(|error| {
            AgentmailError::Other(format!(
                "{operation} outcome is ambiguous after '{original_error}', and a fresh connection for reconciliation failed: {error}; inspect list_mailboxes before retrying"
            ))
        })?;
        let layout = imap_client::list_mailbox_layout(session.session())
            .await
            .map_err(|error| {
                AgentmailError::Other(format!(
                    "{operation} outcome is ambiguous after '{original_error}', and a fresh mailbox listing failed: {error}; inspect list_mailboxes before retrying"
                ))
            })?;
        Ok((session, layout))
    }

    /// APPEND one uniquely identified draft and reconcile a lost completion on
    /// a fresh connection. A generated Message-ID is the idempotency key: its
    /// presence proves the ambiguous APPEND reached the mailbox, while a
    /// failed recovery remains explicitly ambiguous instead of inviting a
    /// duplicate-producing blind retry.
    async fn append_draft_with_recovery(
        &self,
        account: &str,
        drafts_mailbox: &str,
        rfc822: &[u8],
        mut session: connection::PooledSession,
    ) -> Result<Option<(u32, u32)>> {
        let message_id = draft::extract_message_id(rfc822);
        match imap_client::append_draft(session.session(), drafts_mailbox, rfc822).await {
            Ok(()) => {
                let identity = match message_id {
                    Some(message_id) => {
                        imap_client::find_uid_by_message_id(
                            session.session(),
                            drafts_mailbox,
                            &message_id,
                        )
                        .await
                    }
                    None => Ok(None),
                };
                match identity {
                    Ok(identity) => {
                        session.release().await;
                        Ok(identity)
                    }
                    Err(error) => {
                        // APPEND already returned tagged OK, so identity lookup
                        // is best-effort. Never put a timed-out parser back in
                        // the pool merely because the draft itself succeeded.
                        tracing::warn!(
                            target: "agentmail",
                            account,
                            mailbox = drafts_mailbox,
                            error = %error,
                            "draft was appended, but its UID identity could not be recovered"
                        );
                        drop(session);
                        Ok(None)
                    }
                }
            }
            Err(error) if error.is_connection_error() => {
                drop(session);
                let Some(message_id) = message_id else {
                    return Err(AgentmailError::Other(format!(
                        "draft APPEND outcome is ambiguous after {error}; the generated message had no recoverable Message-ID, so inspect Drafts before retrying"
                    )));
                };
                let mut fresh = self.pool.acquire(account).await.map_err(|recovery_error| {
                    AgentmailError::Other(format!(
                        "draft APPEND outcome is ambiguous after {error}, and a fresh recovery connection failed: {recovery_error}; inspect Drafts before retrying"
                    ))
                })?;
                match imap_client::find_uid_by_message_id(
                    fresh.session(),
                    drafts_mailbox,
                    &message_id,
                )
                .await
                {
                    Ok(Some(identity)) => {
                        fresh.release().await;
                        Ok(Some(identity))
                    }
                    Ok(None) => {
                        fresh.release().await;
                        Err(AgentmailError::Other(format!(
                            "draft APPEND outcome is ambiguous after {error}; a fresh Message-ID search found no matching draft, but inspect Drafts before retrying"
                        )))
                    }
                    Err(recovery_error) => Err(AgentmailError::Other(format!(
                        "draft APPEND outcome is ambiguous after {error}, and Message-ID recovery failed: {recovery_error}; inspect Drafts before retrying"
                    ))),
                }
            }
            Err(error) => Err(error),
        }
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
    ) -> Result<Option<String>> {
        if matches!(mode, DeleteMode::Permanent) && !caps.is_gmail() {
            return Ok(None);
        }
        self.special_mailboxes(account, session)
            .await
            .map(|(trash, _)| trash)
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

/// Reject a composed draft that this SERVER would reject at APPEND.
///
/// RFC 7889 `APPENDLIMIT=N` is the largest message a server accepts in one
/// APPEND. Gmail advertises 34 MiB against our own 64 MiB ceiling, so a draft
/// between the two passed our check and failed on the wire — after composing
/// it, after reading every attachment off disk, and (for `update_draft`'s
/// emulated replace) at the one step whose failure the ordering is designed to
/// avoid. Honour whichever bound is lower and name which one it was, so the
/// caller knows whether trimming helps or the server simply will not take it.
///
/// A bare `APPENDLIMIT` (per-mailbox, reported via `STATUS`) yields `None` and
/// leaves our ceiling in force; the server still gets the last word.
fn check_draft_size(len: usize, caps: &imap_client::ServerCaps) -> Result<()> {
    let client_ceiling = MAX_DRAFT_MIME_BYTES as u64;
    let advertised = caps.append_limit();
    let limit = advertised.map_or(client_ceiling, |server| server.min(client_ceiling));
    let len = len as u64;
    if len > limit {
        let source = if advertised.is_some_and(|server| server < client_ceiling) {
            "the server's APPENDLIMIT"
        } else {
            "this client's ceiling"
        };
        return Err(AgentmailError::Other(format!(
            "composed draft is {len} bytes; maximum is {limit} ({source})"
        )));
    }
    Ok(())
}

/// How to dispose of the draft an `update_draft` supersedes, on a server with
/// no RFC 8508 REPLACE.
///
/// `Permanent` is the faithful emulation — REPLACE expunges what it
/// supersedes — but it is only safe where the policy can honor it. Without
/// UIDPLUS a permanent delete degrades to a plain EXPUNGE that would purge
/// every `\Deleted` message in the mailbox, including ones another client
/// flagged, so those servers dispose through Trash instead (recoverable, and
/// needs only MOVE). Gmail takes `Permanent` because `trash_for_mode` routes
/// it to `[Gmail]/Trash` anyway: an in-place EXPUNGE there drops a label and
/// leaves the draft alive in All Mail.
fn discard_mode_for(caps: &imap_client::ServerCaps) -> DeleteMode {
    if caps.is_gmail() || caps.has_uidplus() {
        DeleteMode::Permanent
    } else {
        DeleteMode::TrashFirst
    }
}

#[derive(Debug, Clone, Copy)]
enum MailboxMutationKind {
    Rename,
    Delete,
}

fn normalize_thread_message_id(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .unwrap_or(value)
        .trim();
    if inner.is_empty()
        || !inner.is_ascii()
        || inner
            .chars()
            .any(|character| character.is_ascii_control() || character.is_ascii_whitespace())
    {
        return None;
    }
    Some(format!("<{inner}>"))
}

fn thread_search_value(message_id: &str) -> &str {
    message_id
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .unwrap_or(message_id)
}

fn thread_identity_key(identity: &MailboxMessageIdentity) -> (String, u32, u32) {
    (
        identity.mailbox.clone(),
        identity.uid_validity,
        identity.uid,
    )
}

fn thread_record_message(
    identity: MailboxMessageIdentity,
    info: MessageInfo,
    selection_basis: Vec<String>,
) -> ThreadRecordMessage {
    ThreadRecordMessage {
        identity,
        message_id: info.message_id,
        in_reply_to: info.in_reply_to,
        references: info.references,
        date: info.date,
        from: info.sender,
        subject: info.subject,
        selection_basis,
    }
}

fn queue_thread_ids(
    message: &ThreadRecordMessage,
    known_ids: &mut hashbrown::HashSet<String>,
    pending_ids: &mut std::collections::VecDeque<String>,
) {
    for value in message
        .message_id
        .iter()
        .chain(message.in_reply_to.iter())
        .chain(message.references.iter())
    {
        if let Some(value) = normalize_thread_message_id(value)
            && known_ids.insert(value.clone())
        {
            pending_ids.push_back(value);
        }
    }
}

fn thread_match_basis(message: &MessageInfo, query_id: &str) -> Vec<String> {
    let mut basis = Vec::new();
    if message
        .message_id
        .as_deref()
        .and_then(normalize_thread_message_id)
        .as_deref()
        == Some(query_id)
    {
        basis.push(format!("Message-ID equals {query_id}"));
    }
    if message
        .in_reply_to
        .as_deref()
        .and_then(normalize_thread_message_id)
        .as_deref()
        == Some(query_id)
    {
        basis.push(format!("In-Reply-To equals {query_id}"));
    }
    if message
        .references
        .iter()
        .filter_map(|value| normalize_thread_message_id(value))
        .any(|value| value == query_id)
    {
        basis.push(format!("References contains {query_id}"));
    }
    basis
}

fn push_record_warning(warnings: &mut Vec<String>, warning: String) {
    const MAX_WARNINGS: usize = 20;
    if warnings.len() < MAX_WARNINGS && !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

fn thread_selection_digest(
    account: &str,
    seed: &MailboxMessageIdentity,
    messages: &[ThreadRecordMessage],
) -> Result<String> {
    use sha2::{Digest as _, Sha256};

    let canonical = serde_json::to_vec(&serde_json::json!({
        "schema": "agentmail.thread-selection.v1",
        "account": account,
        "seed": seed,
        "strategy": "exact-rfc-message-id-graph",
        "messages": messages,
    }))
    .map_err(|error| {
        AgentmailError::Other(format!("failed to serialize thread selection: {error}"))
    })?;
    Ok(Sha256::digest(canonical)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn mailbox_names_equal(left: &str, right: &str) -> bool {
    left == right || (left.eq_ignore_ascii_case("INBOX") && right.eq_ignore_ascii_case("INBOX"))
}

fn find_mailbox_layout<'a>(
    layout: &'a [imap_client::MailboxLayout],
    requested: &str,
) -> Option<&'a imap_client::MailboxLayout> {
    layout
        .iter()
        .find(|entry| mailbox_names_equal(&entry.path, requested))
}

fn mailbox_is_same_or_descendant(candidate: &str, parent: &str, delimiter: Option<&str>) -> bool {
    if mailbox_names_equal(candidate, parent) {
        return true;
    }
    let Some(delimiter) = delimiter.filter(|delimiter| !delimiter.is_empty()) else {
        return false;
    };
    candidate
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with(delimiter))
}

async fn mailbox_mutation_preflight(
    session: &mut imap_client::ImapSession,
    mailbox: &imap_client::MailboxLayout,
    layout: &[imap_client::MailboxLayout],
    kind: MailboxMutationKind,
) -> Result<MailboxMutationPreflight> {
    let message_count = if mailbox.is_selectable() {
        imap_client::mailbox_status(session, &mailbox.path, false)
            .await?
            .exists
    } else {
        0
    };
    let mut descendants: Vec<String> = layout
        .iter()
        .filter(|entry| {
            entry.path != mailbox.path
                && mailbox_is_same_or_descendant(
                    &entry.path,
                    &mailbox.path,
                    mailbox.delimiter.as_deref(),
                )
        })
        .map(|entry| entry.path.clone())
        .collect();
    descendants.sort();
    let mut confirmations_required = vec![
        match kind {
            MailboxMutationKind::Rename => "confirmRename=true".to_string(),
            MailboxMutationKind::Delete => "confirmDelete=true".to_string(),
        },
        format!("expectedMessageCount={message_count}"),
    ];
    if matches!(kind, MailboxMutationKind::Delete) && message_count > 0 {
        confirmations_required.push("confirmNonEmpty=true".to_string());
    }
    if !mailbox.roles.is_empty() {
        confirmations_required.push("confirmSpecialUse=true".to_string());
    }
    if !descendants.is_empty() {
        confirmations_required.push("confirmDescendants=true".to_string());
    }
    Ok(MailboxMutationPreflight {
        message_count,
        roles: mailbox.roles.clone(),
        descendants,
        confirmations_required,
    })
}

fn require_expected_message_count(expected: Option<u32>, actual: u32) -> Result<()> {
    match expected {
        Some(expected) if expected == actual => Ok(()),
        Some(expected) => Err(AgentmailError::Other(format!(
            "mailbox message count changed: expected {expected}, current count is {actual}; preview again before confirming"
        ))),
        None => Err(AgentmailError::Other(format!(
            "expectedMessageCount is required for mutation; preview reported {actual}"
        ))),
    }
}

fn canonical_recipient_address(value: &str) -> Option<String> {
    let value = value.trim();
    let candidate = value
        .rfind('<')
        .and_then(|start| value.get(start + 1..))
        .and_then(|tail| tail.strip_suffix('>'))
        .unwrap_or(value)
        .trim();
    crate::config::canonicalize_email(candidate)
}

fn push_reply_recipient(
    output: &mut Vec<String>,
    seen: &mut hashbrown::HashSet<String>,
    own: &hashbrown::HashSet<String>,
    recipient: &str,
) {
    let recipient = recipient.trim();
    if recipient.is_empty() {
        return;
    }
    let identity =
        canonical_recipient_address(recipient).unwrap_or_else(|| recipient.to_ascii_lowercase());
    if own.contains(&identity) || !seen.insert(identity) {
        return;
    }
    output.push(recipient.to_string());
}

fn reply_subject(subject: &str) -> String {
    let subject = subject.trim();
    if subject
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("re:"))
    {
        subject.to_string()
    } else if subject.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {subject}")
    }
}

fn validate_draft_payload(
    body: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    reply_to: &[String],
    attachments: &[DraftAttachment],
) -> Result<()> {
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err(AgentmailError::Other(
            "At least one recipient (to, cc, or bcc) is required".to_string(),
        ));
    }
    let recipient_count = to
        .len()
        .checked_add(cc.len())
        .and_then(|count| count.checked_add(bcc.len()))
        .and_then(|count| count.checked_add(reply_to.len()))
        .ok_or_else(|| AgentmailError::Other("draft recipient count overflow".to_string()))?;
    if recipient_count > MAX_DRAFT_RECIPIENTS {
        return Err(AgentmailError::Other(format!(
            "draft has {recipient_count} recipients; maximum is {MAX_DRAFT_RECIPIENTS}"
        )));
    }
    if body.len() > MAX_DRAFT_BODY_BYTES {
        return Err(AgentmailError::Other(format!(
            "draft body is {} bytes; maximum is {MAX_DRAFT_BODY_BYTES}",
            body.len()
        )));
    }
    if attachments.len() > MAX_DRAFT_ATTACHMENTS {
        return Err(AgentmailError::Other(format!(
            "draft has {} attachments; maximum is {MAX_DRAFT_ATTACHMENTS}",
            attachments.len()
        )));
    }
    let mut total = 0usize;
    for attachment in attachments {
        if attachment.data.len() > MAX_DRAFT_ATTACHMENT_BYTES {
            return Err(AgentmailError::Other(format!(
                "draft attachment '{}' is {} bytes; per-file maximum is {MAX_DRAFT_ATTACHMENT_BYTES}",
                attachment.filename,
                attachment.data.len()
            )));
        }
        total = total
            .checked_add(attachment.data.len())
            .ok_or_else(|| AgentmailError::Other("draft attachment size overflow".to_string()))?;
    }
    if total > MAX_DRAFT_ATTACHMENTS_TOTAL_BYTES {
        return Err(AgentmailError::Other(format!(
            "draft attachments total {total} bytes; maximum is {MAX_DRAFT_ATTACHMENTS_TOTAL_BYTES}"
        )));
    }
    Ok(())
}

/// What a delete sweep matches. The discovery predicate is the only thing that
/// differs across the list-id, exact-sender, and unsubscribe-cleanup deletes;
/// everything else (windowed draining, chunked expunge, per-mailbox tallying,
/// UID-Mode entry) is shared in `delete_sweep`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeleteSelector {
    /// Messages whose List-Id matches (exact). `exact_list_id_uids` normalizes
    /// internally, so the raw header value is accepted here.
    ListId(String),
    /// Every message from one exact sender identity (email + display name),
    /// used by delete-by-sender and move-by-sender.
    Sender { email: String, name: String },
    /// RFC 8058 mail from one exact sender email. A `list_id` further scopes
    /// matches to one normalized List-Id so sibling lists from the same sender
    /// are untouched.
    SubscriptionSender {
        email: String,
        list_id: Option<String>,
    },
    /// A row from `top_subscriptions`: exact canonical sender email plus any
    /// list-action header (`List-Unsubscribe` or `List-Unsubscribe-Post`).
    /// The sample's single normalized List-Id further scopes the move.
    RankedSubscription {
        email: String,
        list_id: Option<String>,
    },
    /// Messages whose first parsed Header From address has this exact
    /// canonical domain. Parent/child domains never match implicitly.
    Domain(String),
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
    pending: usize,
    needs_attention: usize,
    operation_ids: Vec<String>,
}

/// Aggregated outcome of a matching sweep across one or more mailboxes.
#[derive(Debug)]
struct SweepTotals {
    found: usize,
    affected: usize,
    failed: usize,
    pending: usize,
    needs_attention: usize,
    operation_ids: Vec<String>,
    mailboxes: Vec<SweepMailboxTally>,
    skipped: Vec<String>,
    trash_fallback: bool,
    /// False when a mutation response ended in timeout/EOF ambiguity.
    session_usable: bool,
}

impl Default for SweepTotals {
    fn default() -> Self {
        Self {
            found: 0,
            affected: 0,
            failed: 0,
            pending: 0,
            needs_attention: 0,
            operation_ids: Vec::new(),
            mailboxes: Vec::new(),
            skipped: Vec::new(),
            trash_fallback: false,
            session_usable: true,
        }
    }
}

fn delete_tallies(tallies: Vec<SweepMailboxTally>) -> Vec<PerMailboxDeleteResult> {
    tallies
        .into_iter()
        .map(|tally| PerMailboxDeleteResult {
            mailbox: tally.mailbox,
            found: tally.found,
            deleted: tally.affected,
            failed: tally.failed,
            pending: tally.pending,
            needs_attention: tally.needs_attention,
            operation_ids: tally.operation_ids,
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
            pending: tally.pending,
            needs_attention: tally.needs_attention,
            operation_ids: tally.operation_ids,
        })
        .collect()
}

fn pending_move_from_operation(operation: mutation_journal::MoveOperation) -> PendingMove {
    use mutation_journal::MoveJournalState;
    let status = match operation.state {
        MoveJournalState::Complete => MoveStatus::Moved,
        MoveJournalState::CopyFailed => MoveStatus::Failed,
        MoveJournalState::NeedsAttention => MoveStatus::NeedsAttention,
        MoveJournalState::Prepared
        | MoveJournalState::CopyInFlight
        | MoveJournalState::Copied
        | MoveJournalState::DeleteInFlight => MoveStatus::ReconciliationPending,
    };
    PendingMove {
        operation_id: operation.operation_id,
        source_mailbox: operation.source_mailbox,
        source_uid_validity: operation.source_uid_validity,
        source_uid: operation.source_uid,
        destination: operation.destination,
        status,
        detail: operation.detail,
        created_at: operation.created_at,
        updated_at: operation.updated_at,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CleanupIdentity {
    ListId {
        raw: String,
        normalized: String,
    },
    /// Sender-email fallback. Every match must have the exact canonical sender
    /// email plus List-Unsubscribe-Post. `list_id` carries the target's single
    /// normalized List-Id when present, conjoining it with the sender email so
    /// an unauthenticated, spoofable List-Id never selects a delete alone.
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

async fn candidate_sender_uids<T>(
    session: &mut async_imap::Session<T>,
    email: &str,
) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    let sender_has_idn = domain::domain_from_address(email)
        .is_some_and(|domain| domain.split('.').any(|label| label.starts_with("xn--")));
    let criteria = if sender_has_idn {
        // The public sender identity uses an IDNA A-label, while the header may
        // contain the equivalent EAI U-label. Enumerate non-deleted messages,
        // then confirm the canonical address from fetched headers.
        SearchCriteria {
            deleted: Some(false),
            ..Default::default()
        }
    } else {
        SearchCriteria {
            from: Some(email.to_string()),
            deleted: Some(false),
            ..Default::default()
        }
    };
    let query = imap_client::build_search_query_pub(&criteria)?;
    imap_client::search_uids(session, &query).await
}

/// A sender-fallback message must carry RFC 8058's POST header, match the
/// canonical sender email, and satisfy the optional normalized List-Id scope.
fn subscription_sender_header_matches(
    header_bytes: &[u8],
    target_email: &str,
    constrain_list_id: Option<&str>,
) -> bool {
    let header_str = String::from_utf8_lossy(header_bytes);
    if imap_client::extract_header_value_pub(&header_str, "List-Unsubscribe-Post").is_none()
        || !row_list_id_matches(&header_str, constrain_list_id)
    {
        return false;
    }

    parser::parse_sender_date(header_bytes).is_ok_and(|(email, _, _, _)| email == target_email)
}

/// A top-subscription move uses the same bulk-mail eligibility as ranking:
/// either list-action header, exact sender email, and the sample's List-Id
/// when it has one. This intentionally does not weaken the one-click cleanup
/// fallback above, which still requires `List-Unsubscribe-Post`.
fn ranked_subscription_header_matches(
    header_bytes: &[u8],
    target_email: &str,
    constrain_list_id: Option<&str>,
) -> bool {
    let header_str = String::from_utf8_lossy(header_bytes);
    let has_list_action = imap_client::extract_header_value_pub(&header_str, "List-Unsubscribe")
        .is_some()
        || imap_client::extract_header_value_pub(&header_str, "List-Unsubscribe-Post").is_some();
    if !has_list_action || !row_list_id_matches(&header_str, constrain_list_id) {
        return false;
    }

    parser::parse_sender_date(header_bytes).is_ok_and(|(email, _, _, _)| email == target_email)
}

async fn filter_subscription_sender_mail<T>(
    session: &mut async_imap::Session<T>,
    candidate_uids: &[u32],
    target_email: &str,
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
            if subscription_sender_header_matches(header_bytes, target_email, constrain_list_id) {
                exact.push(uid);
            }
        }
    }
    Ok(exact)
}

async fn filter_ranked_subscription_mail<T>(
    session: &mut async_imap::Session<T>,
    candidate_uids: &[u32],
    target_email: &str,
    constrain_list_id: Option<&str>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
{
    let mut exact = Vec::new();
    for chunk in candidate_uids.chunks(1000) {
        imap_client::check_cancel(cancel)?;
        let uid_set = chunk
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetched =
            imap_client::timed_uid_fetch_collect_pub(session, &uid_set, "(UID BODY.PEEK[HEADER])")
                .await?;

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let Some(uid) = fetch.uid else {
                continue;
            };
            if ranked_subscription_header_matches(
                fetch.header().unwrap_or(&[]),
                target_email,
                constrain_list_id,
            ) {
                exact.push(uid);
            }
        }
    }
    Ok(exact)
}

/// Outcome of a page-sample Subject fetch.
struct SampleSubjects {
    /// Decoded Subject per (mailbox, UIDVALIDITY, uid) that the server
    /// returned a row for.
    subjects: hashbrown::HashMap<(String, u32, u32), String>,
    /// Samples whose mailbox FETCH **succeeded** yet returned no row for the
    /// UID — the strongest available deleted-message signal on providers
    /// where external deletions are otherwise invisible (Yahoo/AOL advance
    /// neither UIDNEXT nor a trustworthy EXISTS). Never populated from a
    /// failed EXAMINE or FETCH: an outage must not masquerade as deletion.
    missing: Vec<(String, u32, u32)>,
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
    let mut by_mailbox_epoch: hashbrown::HashMap<(&str, u32), Vec<u32>> = hashbrown::HashMap::new();
    for sample in samples {
        by_mailbox_epoch
            .entry((sample.mailbox.as_str(), sample.uid_validity))
            .or_default()
            .push(sample.uid);
    }

    let mut subjects = hashbrown::HashMap::new();
    let mut missing = Vec::new();
    for ((mailbox, expected_uid_validity), uids) in by_mailbox_epoch {
        if imap_client::check_cancel(cancel).is_err() {
            break;
        }
        let Ok(selected) = imap_client::examine(session, mailbox).await else {
            continue;
        };
        // A UID is meaningful only inside its mailbox epoch. If the mailbox
        // rolled over, neither enrich nor prune this stale sample: UID reuse
        // could otherwise attach an unrelated subject or delete the new
        // epoch's cache membership.
        if selected.uid_validity != Some(expected_uid_validity) {
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
                subjects.insert((mailbox.to_string(), expected_uid_validity, uid), subject);
            }
        }
        missing.extend(
            uids.iter()
                .filter(|uid| !returned.contains(*uid))
                .map(|uid| (mailbox.to_string(), expected_uid_validity, *uid)),
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

/// Require a caller-supplied filename to be one portable path component.
pub(crate) fn validate_plain_filename(filename: &str) -> Result<()> {
    use std::path::{Component, Path};

    if filename.is_empty() || filename.trim() != filename || filename.len() > 240 {
        return Err(AgentmailError::Other(
            "archive filename must be 1..=240 bytes with no surrounding whitespace".to_string(),
        ));
    }
    let mut components = Path::new(filename).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || filename.contains(['/', '\\'])
        || sanitize_filename(filename) != filename
    {
        return Err(AgentmailError::Other(format!(
            "archive filename '{filename}' must be one portable filename with no path separators or reserved characters"
        )));
    }
    Ok(())
}

/// Create, durably write, and close one private file. Any ordinary write error
/// removes the incomplete file; `create_new` is the no-overwrite boundary.
pub(crate) async fn write_new_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            AgentmailError::Other(format!(
                "refusing to overwrite existing message archive '{}'",
                path.display()
            ))
        } else {
            AgentmailError::Other(format!(
                "failed to create message archive '{}': {error}",
                path.display()
            ))
        }
    })?;
    let result = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = result {
        drop(file);
        let _ = tokio::fs::remove_file(path).await;
        return Err(AgentmailError::Other(format!(
            "failed to write message archive '{}': {error}",
            path.display()
        )));
    }
    Ok(())
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
    fn only_inbox_is_case_insensitive_for_mailbox_mutations() {
        assert!(mailbox_names_equal("INBOX", "inbox"));
        assert!(!mailbox_names_equal("Archive", "archive"));
    }

    #[test]
    fn reply_subject_adds_exactly_one_prefix() {
        assert_eq!(reply_subject("Status"), "Re: Status");
        assert_eq!(reply_subject("re: Status"), "re: Status");
        assert_eq!(reply_subject("  RE: Status  "), "RE: Status");
    }

    #[test]
    fn exact_thread_matching_normalizes_brackets_but_not_substrings() {
        let raw = b"From: sender@example.com\r\nSubject: update\r\nMessage-ID: <child@example.com>\r\nIn-Reply-To: <parent@example.com>\r\nReferences: <root@example.com> <parent@example.com>\r\n\r\n";
        let message = parser::parse_rfc822(raw, 1, Vec::new(), None, "INBOX", "work", false, false)
            .expect("parse headers");
        assert_eq!(
            thread_match_basis(&message, "<parent@example.com>"),
            vec![
                "In-Reply-To equals <parent@example.com>".to_string(),
                "References contains <parent@example.com>".to_string(),
            ]
        );
        assert!(thread_match_basis(&message, "<rent@example.com>").is_empty());
        assert_eq!(
            normalize_thread_message_id("parent@example.com").as_deref(),
            Some("<parent@example.com>")
        );
        assert!(normalize_thread_message_id("bad id@example.com").is_none());
    }

    #[test]
    fn archive_filename_requires_one_portable_component() {
        assert!(validate_plain_filename("42.eml").is_ok());
        assert!(validate_plain_filename("Exhibit E - 01.eml").is_ok());
        for invalid in [
            "",
            ".",
            "..",
            "../42.eml",
            "nested/42.eml",
            "a\\b.eml",
            "bad:name.eml",
        ] {
            assert!(
                validate_plain_filename(invalid).is_err(),
                "unexpectedly accepted {invalid:?}"
            );
        }
    }

    #[tokio::test]
    async fn archive_write_is_private_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("agentmail-eml-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.expect("temp dir");
        let path = dir.join("42.eml");
        write_new_private_file(&path, b"first")
            .await
            .expect("first create");
        let error = write_new_private_file(&path, b"second")
            .await
            .expect_err("overwrite must fail");
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(tokio::fs::read(&path).await.expect("read"), b"first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        tokio::fs::remove_dir_all(dir).await.expect("cleanup");
    }

    #[test]
    fn next_offset_handles_full_final_partial_and_past_end_pages() {
        assert_eq!(next_offset(0, 7, 20), Some(7));
        assert_eq!(next_offset(7, 7, 14), None);
        assert_eq!(next_offset(14, 6, 20), None);
        assert_eq!(next_offset(25, 0, 20), None);
    }

    #[test]
    fn own_addresses_returns_lowercased_username() {
        let cfg = Config::from_accounts(vec![(
            "work".to_string(),
            config::AccountConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                username: "Me@Example.COM".to_string(),
                email: None,
                aliases: Vec::new(),
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
        // No usable List-Id → sender email + List-Unsubscribe-Post.
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

    #[test]
    fn subscription_sender_match_ignores_display_name() {
        let headers = b"From: Changed Name <sender@example.com>\r\n\
            List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
            List-Id: News <news.example.com>\r\n\r\n";

        assert!(subscription_sender_header_matches(
            headers,
            "sender@example.com",
            Some("news.example.com")
        ));
    }

    #[test]
    fn subscription_sender_match_requires_list_unsubscribe_post() {
        let headers = b"From: News <sender@example.com>\r\n\
            List-Unsubscribe: <https://example.com/unsubscribe>\r\n\
            List-Id: News <news.example.com>\r\n\r\n";

        assert!(!subscription_sender_header_matches(
            headers,
            "sender@example.com",
            Some("news.example.com")
        ));
    }

    #[test]
    fn subscription_sender_match_rejects_a_sibling_list_id() {
        let headers = b"From: News <sender@example.com>\r\n\
            List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
            List-Id: Other <other.example.com>\r\n\r\n";

        assert!(!subscription_sender_header_matches(
            headers,
            "sender@example.com",
            Some("news.example.com")
        ));
    }

    #[test]
    fn subscription_sender_match_rejects_a_different_email() {
        let headers = b"From: News <other@example.com>\r\n\
            List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
            List-Id: News <news.example.com>\r\n\r\n";

        assert!(!subscription_sender_header_matches(
            headers,
            "sender@example.com",
            Some("news.example.com")
        ));
    }

    #[test]
    fn ranked_subscription_match_accepts_either_list_action_header() {
        let list_unsubscribe = b"From: News <sender@example.com>\r\n\
            List-Unsubscribe: <https://example.com/unsubscribe>\r\n\
            List-Id: News <news.example.com>\r\n\r\n";
        let list_unsubscribe_post = b"From: News <sender@example.com>\r\n\
            List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
            List-Id: News <news.example.com>\r\n\r\n";

        assert!(ranked_subscription_header_matches(
            list_unsubscribe,
            "sender@example.com",
            Some("news.example.com")
        ));
        assert!(ranked_subscription_header_matches(
            list_unsubscribe_post,
            "sender@example.com",
            Some("news.example.com")
        ));
    }

    #[test]
    fn ranked_subscription_match_rejects_ordinary_mail_and_sibling_lists() {
        let ordinary = b"From: News <sender@example.com>\r\n\
            List-Id: News <news.example.com>\r\n\r\n";
        let sibling = b"From: News <sender@example.com>\r\n\
            List-Unsubscribe: <https://example.com/unsubscribe>\r\n\
            List-Id: Other <other.example.com>\r\n\r\n";

        assert!(!ranked_subscription_header_matches(
            ordinary,
            "sender@example.com",
            Some("news.example.com")
        ));
        assert!(!ranked_subscription_header_matches(
            sibling,
            "sender@example.com",
            Some("news.example.com")
        ));
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

    /// Born-UID-Mode defaults to on exactly when the cache is persistent (so the
    /// full-mailbox UID walk is amortized), and `.uidonly(..)` overrides either
    /// way. This is the gate that keeps the no-cache path in classic Limited
    /// Mode while letting cached rankings/reads share one UID-Mode connection.
    #[test]
    fn born_uid_gate_follows_cache_persistence_and_override() {
        let dir = std::env::temp_dir().join("agentmail-uidonly-gate-test");

        // Default: tracks cache persistence.
        let cached = Agentmail::builder(Config::empty()).cache_dir(&dir).build();
        assert!(
            cached.pool.uidonly_enabled(),
            "persistent cache defaults born-UID on"
        );
        let no_cache = Agentmail::builder(Config::empty()).disable_cache().build();
        assert!(
            !no_cache.pool.uidonly_enabled(),
            "no cache defaults born-UID off (windowed Limited path unchanged)"
        );

        // Explicit override wins in both directions.
        let forced_off = Agentmail::builder(Config::empty())
            .cache_dir(&dir)
            .uidonly(false)
            .build();
        assert!(
            !forced_off.pool.uidonly_enabled(),
            ".uidonly(false) overrides a persistent cache"
        );
        let forced_on = Agentmail::builder(Config::empty())
            .disable_cache()
            .uidonly(true)
            .build();
        assert!(
            forced_on.pool.uidonly_enabled(),
            ".uidonly(true) overrides a disabled cache"
        );
    }

    /// The UID-Mode page size is the server's advertised MESSAGELIMIT, or the
    /// default fetch chunk when the server sets no limit.
    #[test]
    fn uid_page_size_uses_messagelimit_else_chunk() {
        let limited = imap_client::ServerCaps::from_strings([
            "UIDONLY".to_string(),
            "MESSAGELIMIT=250".to_string(),
        ]);
        assert_eq!(Agentmail::uid_page_size(&limited), 250);

        let unbounded = imap_client::ServerCaps::from_strings(["UIDONLY".to_string()]);
        assert_eq!(
            Agentmail::uid_page_size(&unbounded),
            imap_client::MAX_FETCH_CHUNK as u32,
            "no MESSAGELIMIT falls back to the default fetch chunk"
        );
    }

    /// A draft the SERVER will refuse must be refused before the wire.
    /// RFC 7889 `APPENDLIMIT=N` is a hard server bound — Gmail's 34 MiB sits
    /// well under this client's own 64 MiB ceiling, so anything between the two
    /// used to pass here and fail at APPEND.
    #[test]
    fn a_draft_is_bounded_by_whichever_of_client_and_server_is_smaller() {
        const MIB: usize = 1024 * 1024;
        // Gmail's real capability line value.
        let gmail = imap_client::ServerCaps::from_strings([
            "IMAP4REV1".to_string(),
            "APPENDLIMIT=35651584".to_string(),
        ]);
        assert!(check_draft_size(30 * MIB, &gmail).is_ok(), "under 34 MiB");
        let error = check_draft_size(40 * MIB, &gmail)
            .expect_err("40 MiB exceeds Gmail's APPENDLIMIT and must not reach the wire");
        let message = error.to_string();
        assert!(
            message.contains("35651584") && message.contains("APPENDLIMIT"),
            "the refusal must name the server's bound, not ours: {message}"
        );

        // iCloud advertises none: our own ceiling is the only bound.
        let icloud =
            imap_client::ServerCaps::from_strings(["IMAP4REV1".to_string(), "UIDPLUS".to_string()]);
        assert!(
            check_draft_size(40 * MIB, &icloud).is_ok(),
            "40 MiB is fine where the server declares no limit"
        );
        let error = check_draft_size(70 * MIB, &icloud).expect_err("70 MiB exceeds our ceiling");
        assert!(
            error.to_string().contains("this client's ceiling"),
            "with no server bound the refusal is ours to own: {error}"
        );

        // A BARE `APPENDLIMIT` means per-mailbox limits reported via STATUS —
        // unknown here, so it must not be read as "no limit" OR as zero.
        let bare = imap_client::ServerCaps::from_strings([
            "IMAP4REV1".to_string(),
            "APPENDLIMIT".to_string(),
        ]);
        assert!(check_draft_size(40 * MIB, &bare).is_ok(), "unknown ≠ zero");

        // A server MORE generous than we are does not raise our ceiling.
        let generous = imap_client::ServerCaps::from_strings([
            "IMAP4REV1".to_string(),
            "APPENDLIMIT=999999999".to_string(),
        ]);
        assert!(check_draft_size(70 * MIB, &generous).is_err());
    }

    /// The safety half of the APPEND-then-discard emulation. `Permanent`
    /// mirrors REPLACE, but on a server without UIDPLUS it degrades to a plain
    /// EXPUNGE that would purge every `\Deleted` message in Drafts — including
    /// ones another client flagged. Those servers must dispose through Trash.
    #[test]
    fn a_superseded_draft_is_only_expunged_where_uidplus_makes_it_targeted() {
        let icloud = imap_client::ServerCaps::from_strings([
            "IMAP4REV1".to_string(),
            "UIDPLUS".to_string(),
            "MOVE".to_string(),
        ]);
        assert_eq!(
            discard_mode_for(&icloud),
            DeleteMode::Permanent,
            "UIDPLUS makes UID EXPUNGE targeted, so REPLACE's semantics are safe to mirror"
        );

        // No UIDPLUS: permanent mode would reach a plain EXPUNGE.
        let plain = imap_client::ServerCaps::from_strings([
            "IMAP4REV1".to_string(),
            "MOVE".to_string(),
        ]);
        assert_eq!(
            discard_mode_for(&plain),
            DeleteMode::TrashFirst,
            "without UIDPLUS the discard must go to Trash, never a blanket EXPUNGE"
        );

        // Gmail: `trash_for_mode` routes Permanent to [Gmail]/Trash, because an
        // in-place EXPUNGE there only drops a label.
        let gmail = imap_client::ServerCaps::from_strings([
            "IMAP4REV1".to_string(),
            "X-GM-EXT-1".to_string(),
        ]);
        assert_eq!(
            discard_mode_for(&gmail),
            DeleteMode::Permanent,
            "Gmail is routed to its Trash by trash_for_mode even in permanent mode"
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
            joined.contains("UID STORE 5 +FLAGS.SILENT (\\Deleted)"),
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

    #[tokio::test]
    async fn domain_sweep_confirms_exact_domain_and_excludes_child_and_own_sender() {
        let (mut session, server) = scripted_sweep_session(" 5 7 9", |tag| {
            let target = "From: Sender <sender@example.com>\r\n\r\n";
            let child = "From: Child <sender@mail.example.com>\r\n\r\n";
            let own = "From: Me <me@example.com>\r\n\r\n";
            format!(
                "* 1 FETCH (UID 5 BODY[HEADER.FIELDS (FROM)] {{{}}}\r\n{target})\r\n* 2 FETCH (UID 7 BODY[HEADER.FIELDS (FROM)] {{{}}}\r\n{child})\r\n* 3 FETCH (UID 9 BODY[HEADER.FIELDS (FROM)] {{{}}}\r\n{own})\r\n{tag} OK FETCH completed\r\n",
                target.len(),
                child.len(),
                own.len()
            )
        })
        .await;
        let config = Config::from_accounts(vec![(
            "test-account".to_string(),
            AccountConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                username: "login".to_string(),
                email: Some("me@example.com".to_string()),
                aliases: Vec::new(),
                password: None,
                tls: true,
                max_connections: None,
                auth: AuthMethod::Password,
            },
        )]);
        let mk = Agentmail::builder(config).disable_cache().build();
        let caps = imap_client::ServerCaps::from_strings(["UIDPLUS".to_string()]);
        let totals = mk
            .matching_sweep_loop(
                &mut session,
                "test-account",
                &DeleteSelector::Domain("example.com".to_string()),
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
            .expect("scripted domain sweep succeeds");
        assert_eq!(totals.found, 1);
        assert_eq!(totals.affected, 1);

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        let joined = commands.concat();
        assert!(joined.contains("UID STORE 5 +FLAGS.SILENT (\\Deleted)"));
        assert!(!joined.contains("UID STORE 5,7"));
        assert!(!joined.contains("UID STORE 5,7,9"));
    }

    #[tokio::test]
    async fn idn_domain_sweep_enumerates_undeleted_then_confirms_u_label() {
        let (mut session, server) = scripted_sweep_session(" 11 12", |tag| {
            let target = "From: Reader <reader@bücher.de>\r\n\r\n";
            let child = "From: Child <reader@mail.bücher.de>\r\n\r\n";
            format!(
                "* 1 FETCH (UID 11 BODY[HEADER.FIELDS (FROM)] {{{}}}\r\n{target})\r\n* 2 FETCH (UID 12 BODY[HEADER.FIELDS (FROM)] {{{}}}\r\n{child})\r\n{tag} OK FETCH completed\r\n",
                target.len(),
                child.len()
            )
        })
        .await;
        let mk = Agentmail::builder(Config::empty()).disable_cache().build();
        let caps = imap_client::ServerCaps::from_strings(["UIDPLUS".to_string()]);
        let totals = mk
            .matching_sweep_loop(
                &mut session,
                "test-account",
                &DeleteSelector::Domain("xn--bcher-kva.de".to_string()),
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
            .expect("scripted IDN domain sweep succeeds");
        assert_eq!(totals.found, 1);
        assert_eq!(totals.affected, 1);

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        let joined = commands.concat();
        assert!(joined.contains("UID SEARCH UNDELETED"));
        assert!(!joined.contains("FROM \"@xn--bcher-kva.de\""));
        assert!(joined.contains("UID STORE 11 +FLAGS.SILENT (\\Deleted)"));
    }

    /// The windowed-drain loop's reason to exist: on a server whose visible
    /// window backfills after each delete, the loop must re-select and keep
    /// deleting across MULTIPLE productive passes, aggregating the found/
    /// affected counts, then terminate on the first empty pass. Here passes 1
    /// and 2 each surface one fresh matching UID (5, then 6); pass 3 is empty.
    #[tokio::test]
    async fn matching_sweep_loop_drains_a_windowed_mailbox_across_passes() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        // One fresh matching UID per productive pass; None (=> empty) ends it.
        let uid_for_pass = [5u32, 6u32];
        let server = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();
            let mut selects = 0usize;
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
                    // A message stays visible while any pass still has one to
                    // surface (models the window backfilling); then empties.
                    let exists = if selects <= uid_for_pass.len() { 1 } else { 0 };
                    format!(
                        "* {exists} EXISTS\r\n* OK [UIDVALIDITY 9] v\r\n* OK [UIDNEXT 100] n\r\n{tag} OK [READ-WRITE] SELECT completed\r\n"
                    )
                } else if line.contains("UID SEARCH") {
                    let hits = uid_for_pass
                        .get(selects.saturating_sub(1))
                        .map(|u| format!(" {u}"))
                        .unwrap_or_default();
                    format!("* SEARCH{hits}\r\n{tag} OK SEARCH completed\r\n")
                } else if line.contains("UID FETCH") {
                    // Confirm the current pass's UID carries the target List-Id.
                    let uid = uid_for_pass[selects.saturating_sub(1)];
                    let target = "List-Id: News <news.example.com>\r\n\r\n";
                    format!(
                        "* 1 FETCH (UID {uid} BODY[HEADER.FIELDS (LIST-ID)] {{{}}}\r\n{target})\r\n{tag} OK FETCH completed\r\n",
                        target.len()
                    )
                } else if line.contains("UID STORE") {
                    format!("{tag} OK STORE completed\r\n")
                } else if line.contains("UID EXPUNGE") {
                    format!("* 1 EXPUNGE\r\n{tag} OK EXPUNGE completed\r\n")
                } else if line.contains("NOOP") {
                    format!("{tag} OK NOOP completed\r\n")
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
            .expect("multi-pass windowed sweep succeeds");

        // Counts aggregate across both productive passes.
        assert_eq!(totals.found, 2, "one match per productive pass, summed");
        assert_eq!(totals.affected, 2, "both deletes counted across passes");
        assert_eq!(totals.failed, 0);
        assert!(
            totals.skipped.is_empty(),
            "the mailbox drained; it must not be reported skipped"
        );
        assert_eq!(totals.mailboxes.len(), 1);
        assert_eq!(totals.mailboxes[0].found, 2);

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        assert_eq!(
            commands.iter().filter(|c| c.contains("SELECT")).count(),
            3,
            "two productive passes + one empty terminating pass: {commands:?}"
        );
        let joined = commands.concat();
        assert!(
            joined.contains("UID EXPUNGE 5") && joined.contains("UID EXPUNGE 6"),
            "each window's fresh UID is expunged in its own pass: {commands:?}"
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
            subjects
                .get(&("INBOX".to_string(), 9, 5))
                .map(String::as_str),
            Some("Your July statement is ready")
        );
        assert_eq!(
            subjects
                .get(&("INBOX".to_string(), 9, 7))
                .map(String::as_str),
            Some("Résumé"),
            "RFC 2047 encoded-words are decoded"
        );
        assert!(
            !subjects.contains_key(&("INBOX".to_string(), 9, 9)),
            "a stale sample yields no subject instead of an error"
        );
        assert_eq!(
            missing,
            vec![("INBOX".to_string(), 9, 9)],
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

    #[tokio::test]
    async fn sample_subjects_never_fetches_or_prunes_across_uidvalidity_rollover() {
        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
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
                        "* 1 EXISTS\r\n* OK [UIDVALIDITY 10] new epoch\r\n* OK [UIDNEXT 6] next\r\n{tag} OK [READ-ONLY] EXAMINE completed\r\n"
                    )
                } else {
                    panic!("UID FETCH must not run across a UIDVALIDITY mismatch: {line:?}");
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

        let sample = MailboxMessageIdentity {
            mailbox: "INBOX".to_string(),
            uid_validity: 9,
            uid: 5,
        };
        let SampleSubjects { subjects, missing } =
            sample_subjects(&mut session, &[sample], None).await;
        assert!(subjects.is_empty());
        assert!(
            missing.is_empty(),
            "an epoch mismatch is not proof that a UID is missing"
        );

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains("UID FETCH"))
                .count(),
            0
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

    /// End-to-end sender-fallback sweep with the pertinence constraint.
    #[tokio::test]
    async fn delete_sweep_sender_fallback_is_scoped_to_the_constrained_list_id() {
        let (mut session, server) = scripted_sweep_session(" 11 12 13", |tag| {
            let matching = "From: Changed Name <sender@example.com>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nList-Id: News <news.example.com>\r\n\r\n";
            let sibling = "From: News <sender@example.com>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nList-Id: Other <other.example.com>\r\n\r\n";
            let no_post = "From: News <sender@example.com>\r\nList-Unsubscribe: <https://x.example/u>\r\nList-Id: News <news.example.com>\r\n\r\n";
            format!(
                "* 1 FETCH (UID 11 BODY[HEADER] {{{}}}\r\n{matching})\r\n* 2 FETCH (UID 12 BODY[HEADER] {{{}}}\r\n{sibling})\r\n* 3 FETCH (UID 13 BODY[HEADER] {{{}}}\r\n{no_post})\r\n{tag} OK FETCH completed\r\n",
                matching.len(),
                sibling.len(),
                no_post.len()
            )
        })
        .await;

        let mk = Agentmail::new(Config::empty());
        let caps = imap_client::ServerCaps::from_strings(["UIDPLUS".to_string()]);
        let totals = mk
            .matching_sweep_loop(
                &mut session,
                "test-account",
                &DeleteSelector::SubscriptionSender {
                    email: "sender@example.com".to_string(),
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
            joined.contains("UID STORE 11 +FLAGS.SILENT (\\Deleted)"),
            "deletes only the constrained match: {commands:?}"
        );
        assert!(
            !joined.contains("UID STORE 11,12") && !joined.contains("UID STORE 11,12,13"),
            "the sibling-list and missing-POST messages must survive: {commands:?}"
        );
    }

    #[tokio::test]
    async fn move_subscription_sweep_matches_ranked_bulk_mail_scope() {
        let (mut session, server) = scripted_sweep_session(" 11 12 13", |tag| {
            let matching = "From: Changed Name <sender@example.com>\r\nList-Unsubscribe: <https://x.example/u>\r\nList-Id: News <news.example.com>\r\n\r\n";
            let sibling = "From: News <sender@example.com>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nList-Id: Other <other.example.com>\r\n\r\n";
            let ordinary = "From: News <sender@example.com>\r\nList-Id: News <news.example.com>\r\n\r\n";
            format!(
                "* 1 FETCH (UID 11 BODY[HEADER] {{{}}}\r\n{matching})\r\n* 2 FETCH (UID 12 BODY[HEADER] {{{}}}\r\n{sibling})\r\n* 3 FETCH (UID 13 BODY[HEADER] {{{}}}\r\n{ordinary})\r\n{tag} OK FETCH completed\r\n",
                matching.len(),
                sibling.len(),
                ordinary.len()
            )
        })
        .await;

        let mail = Agentmail::new(Config::empty());
        let caps = imap_client::ServerCaps::from_strings(["MOVE".to_string()]);
        let totals = mail
            .matching_sweep_loop(
                &mut session,
                "test-account",
                &DeleteSelector::RankedSubscription {
                    email: "sender@example.com".to_string(),
                    list_id: Some("news.example.com".to_string()),
                },
                &["INBOX".to_string()],
                SweepAction::Move {
                    destination: "Subscriptions",
                },
                &caps,
                None,
                None,
            )
            .await
            .expect("scripted subscription move succeeds");

        assert_eq!(totals.found, 1);
        assert_eq!(totals.affected, 1);
        assert_eq!(totals.failed, 0);

        drop(session);
        let commands = server.await.expect("scripted server finishes");
        let joined = commands.concat();
        assert!(
            joined.contains("UID MOVE 11"),
            "moves only the exact ranked-subscription match: {commands:?}"
        );
        assert!(
            !joined.contains("UID MOVE 11,12") && !joined.contains("UID MOVE 11,12,13"),
            "sibling-list and ordinary mail must stay put: {commands:?}"
        );
    }
}
