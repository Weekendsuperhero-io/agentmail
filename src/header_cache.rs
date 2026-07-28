//! Persistent UID membership and immutable ranking-header cache.
//!
//! The cache never stores complete messages, bodies, subjects, recipients,
//! flags, or credentials. Correctness is rooted in the RFC identity tuple
//! `(mailbox, UIDVALIDITY, UID)`; SQLite is only an optimization around live
//! IMAP validation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Instant;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::AgentmailError;
use crate::config::AccountConfig;
use crate::domain::{canonicalize_domain, domain_from_address};
use crate::imap_client::{self, CancelFn, ImapSession, ListHeaderRow, ProgressFn};
use crate::scan_cache::MailboxStatus;

// v7: canonicalizes sender email domains to their UTS #46 ASCII form so own
// identity exclusion and domain actions agree for IDNs. v6 added the derived,
// token-free sender domain. Both upgrades backfill in place without IMAP I/O.
// v5: the cache key dropped the account display name (identity is now
// host/port/tls/username only), so existing name-keyed rows are orphaned —
// wipe and rebuild once under the new key. (v4 added mailbox_state's account
// mutation revision so local deletes/moves invalidate snapshot hits even when
// Yahoo/AOL's pinned EXISTS window leaves the (UIDVALIDITY, UIDNEXT, EXISTS)
// triple unchanged.)
const CACHE_SCHEMA_VERSION: i64 = 7;
// v4: rebuilds projections poisoned by HEADER.FIELDS-filtering servers
// (AOL/Yahoo omit List-Unsubscribe[-Post] → has_list_headers was 0 on every
// row despite bulk mail being present).
// v5: discards UID-Mode memberships written before the PARTIAL walk fix. The
// old walk advanced the PARTIAL offset (which Yahoo/AOL cap at one window), so
// it stored a truncated membership of the newest ~1000 UIDs and every rank came
// from that sliver. The fixed walk shrinks the UID range instead and covers the
// whole mailbox; bumping here forces the poisoned single-window memberships to
// cold-rebuild rather than being served forever as a completeness-passing hit.
const HEADER_PROJECTION_VERSION: i64 = 5;
const FETCH_CHUNK_SIZE: usize = 1_000;
const MAX_STABLE_SEARCH_ATTEMPTS: usize = 3;
const MAILING_LIST_SENDER_PREVIEW_LIMIT: usize = 5;

type CacheResult<T> = std::result::Result<T, CacheError>;

#[derive(Debug, thiserror::Error)]
enum CacheError {
    #[error("cache I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cache blocking task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("cache publication conflicted with a newer snapshot")]
    Conflict,
    #[error("cache publication cancelled")]
    Cancelled,
    #[error("cache invariant failed: {0}")]
    Invariant(String),
}

#[derive(Debug, thiserror::Error)]
enum CacheSyncError {
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error(transparent)]
    Mail(#[from] AgentmailError),
}

#[derive(Debug, Clone, Copy)]
struct StoredState {
    uid_validity: u32,
    uid_next: Option<u32>,
    exists: u32,
    /// Account mutation revision this snapshot was published under. A later
    /// fence bump makes the snapshot stale regardless of the mailbox triple.
    account_revision: i64,
}

#[derive(Debug, Clone, Copy)]
struct LoadedState {
    mailbox: Option<StoredState>,
    existing_revision: Option<i64>,
    account_revision: i64,
    /// Persisted server-quirk flag: HEADER.FIELDS responses filter List-*
    /// headers, so syncs must fetch full headers (see `account_quirks`).
    header_fields_filtered: bool,
}

/// Stable mailbox-local identity for a representative ranking message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedRankSample {
    pub(crate) mailbox: String,
    pub(crate) uid_validity: u32,
    pub(crate) uid: u32,
    pub(crate) date: Option<DateTime<Utc>>,
}

/// A bounded page from a SQL-backed ranking query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedRankPage<T> {
    pub(crate) total_messages: u64,
    pub(crate) total_groups: u64,
    pub(crate) items: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedSenderRank {
    pub(crate) address: String,
    pub(crate) display_name: String,
    pub(crate) count: u64,
    pub(crate) oldest_date: Option<DateTime<Utc>>,
    pub(crate) newest_date: Option<DateTime<Utc>>,
    pub(crate) sample: CachedRankSample,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedDomainRank {
    /// Exact canonical ASCII domain, distinct from both its parent and child
    /// domains (for example, `example.com` and `mail.example.com`).
    pub(crate) domain: String,
    pub(crate) count: u64,
    pub(crate) oldest_date: Option<DateTime<Utc>>,
    pub(crate) newest_date: Option<DateTime<Utc>>,
    pub(crate) sample: CachedRankSample,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedSubscriptionRank {
    pub(crate) address: String,
    pub(crate) count: u64,
    pub(crate) oldest_date: Option<DateTime<Utc>>,
    pub(crate) newest_date: Option<DateTime<Utc>>,
    pub(crate) advertised_one_click: bool,
    pub(crate) sample: CachedRankSample,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedMailingListRank {
    /// Canonical RFC 2919 identifier without the surrounding angle brackets.
    pub(crate) list_id: String,
    pub(crate) display_name: String,
    pub(crate) senders: Vec<String>,
    pub(crate) sender_count: u64,
    pub(crate) count: u64,
    pub(crate) oldest_date: Option<DateTime<Utc>>,
    pub(crate) newest_date: Option<DateTime<Utc>>,
    pub(crate) sample: CachedRankSample,
}

#[derive(Debug, Clone)]
struct RankScope {
    path: Arc<PathBuf>,
    account: String,
    mailboxes: Vec<String>,
}

#[derive(Debug, Clone)]
struct CacheKey {
    account: String,
    mailbox: String,
}

impl CacheKey {
    fn new(account_name: &str, config: &AccountConfig, mailbox: &str) -> Self {
        // The cache identity is the SERVER mailbox — host, port, TLS mode, and
        // login — NOT the user-chosen display name. Renaming an account
        // (e.g. "Custom" → "Cthrower") points at the same mailbox and must
        // reuse the same projection, not spin up a fresh (cold, and possibly
        // FIELDS-poisoned) namespace. Length prefixes avoid ambiguous
        // separators while keeping the key inspectable in the cache file.
        let _ = account_name;
        let host = config.host.to_ascii_lowercase();
        let account = format!(
            "{}:{}|{}|{}|{}:{}",
            host.len(),
            host,
            config.port,
            u8::from(config.tls),
            config.username.len(),
            config.username
        );
        Self {
            account,
            mailbox: normalize_mailbox(mailbox),
        }
    }

    fn gate_key(&self) -> String {
        format!("{}\0{}", self.account, self.mailbox)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncKind {
    Cold,
    Hit,
    Tail,
    Membership,
}

impl SyncKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Hit => "hit",
            Self::Tail => "tail",
            Self::Membership => "membership",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishOutcome {
    Published,
    Conflict,
}

/// Persistent header cache used by account ranking operations.
pub(crate) struct HeaderCache {
    path: Option<Arc<PathBuf>>,
    gates: parking_lot::Mutex<HashMap<String, Weak<Mutex<()>>>>,
    /// Accounts whose server filters List-Unsubscribe out of HEADER.FIELDS
    /// responses (AOL/Yahoo). Once detected, syncs for the account go
    /// straight to full-header fetches. Process-local by design: detection
    /// is cheap and re-runs after a restart.
    quirky_accounts: parking_lot::Mutex<HashSet<String>>,
}

impl Default for HeaderCache {
    fn default() -> Self {
        Self {
            path: default_cache_path().map(Arc::new),
            gates: parking_lot::Mutex::new(HashMap::new()),
            quirky_accounts: parking_lot::Mutex::new(HashSet::new()),
        }
    }
}

impl HeaderCache {
    /// A cache persisted at exactly this SQLite file path. Used by the
    /// embedding builder (`Agentmail::builder(..).cache_dir(..)`) and tests;
    /// unlike [`Default`], environment variables are not consulted.
    pub(crate) fn at_path(path: PathBuf) -> Self {
        Self {
            path: Some(Arc::new(path)),
            gates: parking_lot::Mutex::new(HashMap::new()),
            quirky_accounts: parking_lot::Mutex::new(HashSet::new()),
        }
    }

    /// A disabled cache: nothing persists, `is_persistent()` is false, and UID
    /// Mode is never entered (the Limited-Mode live fallback stays in use).
    pub(crate) fn disabled() -> Self {
        Self {
            path: None,
            gates: parking_lot::Mutex::new(HashMap::new()),
            quirky_accounts: parking_lot::Mutex::new(HashSet::new()),
        }
    }

    /// The versioned cache file name, shared by [`Default`] (under the env or
    /// OS cache root) and the builder's explicit directory.
    pub(crate) const FILE_NAME: &'static str = "header-cache-v1.sqlite3";

    /// Whether a persistent projection is available. UID Mode routes the
    /// whole-mailbox walk through the cache, so callers only enter it when
    /// this is true — otherwise the Limited-Mode live fallback stays usable.
    pub(crate) fn is_persistent(&self) -> bool {
        self.path.is_some()
    }

    fn account_is_quirky(&self, account_key: &str) -> bool {
        self.quirky_accounts.lock().contains(account_key)
    }

    fn mark_account_quirky(&self, account_key: &str) {
        self.quirky_accounts.lock().insert(account_key.to_string());
    }

    /// Remember the quirk in-process AND on disk, so a restart cannot
    /// silently drop back to the FIELDS fetch mode this server corrupts —
    /// small post-restart tail syncs would misclassify new bulk mail
    /// without ever re-tripping the detection threshold. Best-effort: a
    /// write failure only loses the persistence, not the in-process flag.
    async fn persist_account_quirky(&self, account_key: &str) {
        self.mark_account_quirky(account_key);
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let path = Arc::clone(path);
        let account = account_key.to_string();
        let write = tokio::task::spawn_blocking(move || -> CacheResult<()> {
            let connection = open_connection(&path)?;
            connection.execute(
                "INSERT INTO account_quirks (account_key, header_fields_filtered)
                 VALUES (?1, 1)
                 ON CONFLICT (account_key) DO UPDATE SET header_fields_filtered = 1",
                params![account],
            )?;
            Ok(())
        })
        .await;
        if let Ok(Err(error)) = write {
            warn!(
                target: "agentmail",
                error = %error,
                "could not persist the HEADER.FIELDS quirk flag"
            );
        }
    }

    /// Whether scans flagged this account's server as filtering List-*
    /// headers (see `mark_account_quirky`). Deletion flows use this to
    /// decide when a zero-result `SEARCH HEADER List-Id` cannot be trusted:
    /// the same backends that filter the headers cannot match them in SEARCH.
    ///
    /// Process-local and effectively one-shot: detection happens during the
    /// healing sync, after which cache hits never re-run it — so a restart
    /// unarms it. Deletion flows therefore combine it with the persisted
    /// evidence in [`Self::cached_list_id_count`].
    pub(crate) fn account_flagged_quirky(
        &self,
        account_name: &str,
        config: &AccountConfig,
    ) -> bool {
        let key = CacheKey::new(account_name, config, "");
        self.account_is_quirky(&key.account)
    }

    /// Whether a mailbox's stored projection shows the HEADER.FIELDS-filter
    /// poisoning: many rows carry a `List-Id` (bulk mail is clearly present)
    /// yet not one row has an unsubscribe flag. This happens when a scan built
    /// the projection in FIELDS mode before the quirk was known (e.g. an
    /// interrupted cold scan, or a fresh namespace after an account rename) —
    /// the persisted quirk flag then only steers FUTURE fetches, leaving the
    /// already-stored rows stuck at `has_list_headers = 0` forever, because
    /// incremental syncs never re-fetch them. Detecting it from stored state
    /// (not just this sync's fresh batch) is what triggers the heal.
    async fn mailbox_projection_poisoned(
        &self,
        path: &Arc<PathBuf>,
        key: &CacheKey,
        uid_validity: u32,
    ) -> bool {
        let path = Arc::clone(path);
        let key = key.clone();
        let counts = tokio::task::spawn_blocking(move || -> CacheResult<(i64, i64)> {
            let connection = open_connection(&path)?;
            Ok(connection.query_row(
                "SELECT
                     COALESCE(SUM(CASE WHEN list_id IS NOT NULL THEN 1 ELSE 0 END), 0),
                     COALESCE(SUM(has_list_headers), 0)
                   FROM header_rows
                  WHERE account_key = ?1 AND mailbox = ?2
                    AND uid_validity = ?3 AND projection_version = ?4",
                params![
                    key.account,
                    key.mailbox,
                    i64::from(uid_validity),
                    HEADER_PROJECTION_VERSION
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )?)
        })
        .await;
        match counts {
            Ok(Ok((with_list_id, with_unsub))) => imap_client::header_fields_quirk(
                usize::try_from(with_list_id).unwrap_or(0),
                usize::try_from(with_unsub).unwrap_or(0),
            ),
            _ => false,
        }
    }

    /// Remove one message's projection row and membership marker — the
    /// self-heal for a stale ranking sample. The server said the UID no
    /// longer exists, but external deletions on Yahoo/AOL advance neither
    /// UIDNEXT nor a trustworthy EXISTS, so the cache cannot notice them on
    /// its own. Both tables are pruned together so the covered==membership
    /// completeness yardstick stays intact and the next ranking call simply
    /// picks a different sample. Best-effort — a cache failure only defers
    /// the heal to the next full resync.
    pub(crate) async fn prune_uid(
        &self,
        account_name: &str,
        config: &AccountConfig,
        mailbox: &str,
        uid_validity: u32,
        uid: u32,
    ) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let key = CacheKey::new(account_name, config, mailbox);
        if let Err(error) = prune_uid_row(Arc::clone(path), key, uid_validity, uid).await {
            debug!(
                target: "agentmail",
                error = %error,
                uid,
                "failed to prune a stale cache row; the next resync heals it"
            );
        }
    }

    /// The projected UIDs in one mailbox epoch that carry this normalized
    /// List-Id. Restart-proof ground truth for deletion flows: when the
    /// server's `SEARCH HEADER List-Id` returns nothing while the projection
    /// knows matches exist, the search is blind — and these UIDs are a far
    /// cheaper candidate set than enumerating a 100k-message window. Stale
    /// entries are harmless (the caller's confirm fetch drops them). Returns
    /// empty when the cache is disabled or unreadable.
    pub(crate) async fn cached_list_id_uids(
        &self,
        account_name: &str,
        config: &AccountConfig,
        mailbox: &str,
        normalized_list_id: &str,
        uid_validity: u32,
    ) -> Vec<u32> {
        let Some(path) = self.path.as_ref() else {
            return Vec::new();
        };
        let key = CacheKey::new(account_name, config, mailbox);
        let normalized = normalized_list_id.to_string();
        let path = Arc::clone(path);
        let uids = tokio::task::spawn_blocking(move || -> CacheResult<Vec<u32>> {
            let connection = open_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT uid FROM header_rows
                  WHERE account_key = ?1 AND mailbox = ?2
                    AND projection_version = ?3 AND uid_validity = ?4
                    AND list_id = ?5
                  ORDER BY uid",
            )?;
            let rows = statement.query_map(
                params![
                    key.account,
                    key.mailbox,
                    HEADER_PROJECTION_VERSION,
                    i64::from(uid_validity),
                    normalized
                ],
                |row| row.get::<_, i64>(0),
            )?;
            let mut uids = Vec::new();
            for row in rows {
                uids.push(sql_u32(row?)?);
            }
            Ok(uids)
        })
        .await;
        match uids {
            Ok(Ok(uids)) => uids,
            _ => Vec::new(),
        }
    }

    /// Cached candidates whose header `From` address has this exact canonical
    /// domain. The caller must confirm each candidate from live headers before
    /// mutating it; stale cache entries therefore cannot broaden an action.
    /// Returns empty when the domain is invalid or the cache is unavailable.
    pub(crate) async fn cached_domain_uids(
        &self,
        account_name: &str,
        config: &AccountConfig,
        mailbox: &str,
        domain: &str,
        uid_validity: u32,
    ) -> Vec<u32> {
        let Some(path) = self.path.as_ref() else {
            return Vec::new();
        };
        let Some(domain) = canonicalize_domain(domain) else {
            return Vec::new();
        };
        let key = CacheKey::new(account_name, config, mailbox);
        let path = Arc::clone(path);
        let uids = tokio::task::spawn_blocking(move || -> CacheResult<Vec<u32>> {
            let connection = open_connection(&path)?;
            let mut statement = connection.prepare(
                "SELECT h.uid
                   FROM header_rows AS h
                   JOIN mailbox_state AS state
                     ON state.account_key = h.account_key
                    AND state.mailbox = h.mailbox
                    AND state.uid_validity = h.uid_validity
                    AND state.projection_version = h.projection_version
                   JOIN membership AS m
                     ON m.account_key = h.account_key
                    AND m.mailbox = h.mailbox
                    AND m.uid = h.uid
                  WHERE h.account_key = ?1 AND h.mailbox = ?2
                    AND h.projection_version = ?3 AND h.uid_validity = ?4
                    AND h.sender_domain = ?5
                  ORDER BY h.uid",
            )?;
            let rows = statement.query_map(
                params![
                    key.account,
                    key.mailbox,
                    HEADER_PROJECTION_VERSION,
                    i64::from(uid_validity),
                    domain
                ],
                |row| row.get::<_, i64>(0),
            )?;
            let mut uids = Vec::new();
            for row in rows {
                uids.push(sql_u32(row?)?);
            }
            Ok(uids)
        })
        .await;
        match uids {
            Ok(Ok(uids)) => uids,
            _ => Vec::new(),
        }
    }

    fn gate(&self, key: &CacheKey) -> Arc<Mutex<()>> {
        let gate_key = key.gate_key();
        let mut gates = self.gates.lock();
        if let Some(gate) = gates.get(&gate_key).and_then(Weak::upgrade) {
            return gate;
        }
        if gates.len() >= 64 {
            gates.retain(|_, gate| gate.strong_count() > 0);
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(gate_key, Arc::downgrade(&gate));
        gate
    }

    /// Synchronize the selected mailboxes, then aggregate a bounded sender
    /// page in SQLite. `None` asks the caller to use its live in-memory
    /// fallback because the local optimization was disabled or unavailable.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn top_senders_page(
        &self,
        session: &mut ImapSession,
        account_name: &str,
        config: &AccountConfig,
        mailboxes: &[String],
        uid_mode: Option<u32>,
        own_addresses: &[String],
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> crate::Result<Option<CachedRankPage<CachedSenderRank>>> {
        let Some(scope) = self
            .validated_rank_scope(
                session,
                account_name,
                config,
                mailboxes,
                uid_mode,
                on_progress,
                cancel,
            )
            .await?
        else {
            return Ok(None);
        };
        let own_addresses = own_addresses.to_vec();
        match query_sender_page(scope, own_addresses, offset, limit).await {
            Ok(page) => Ok(Some(page)),
            Err(error) => {
                warn!(target: "agentmail", error = %error, "sender cache query unavailable");
                Ok(None)
            }
        }
    }

    /// Synchronize the selected mailboxes, then aggregate a bounded page of
    /// exact canonical sender domains in SQLite.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn top_domains_page(
        &self,
        session: &mut ImapSession,
        account_name: &str,
        config: &AccountConfig,
        mailboxes: &[String],
        uid_mode: Option<u32>,
        own_addresses: &[String],
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> crate::Result<Option<CachedRankPage<CachedDomainRank>>> {
        let Some(scope) = self
            .validated_rank_scope(
                session,
                account_name,
                config,
                mailboxes,
                uid_mode,
                on_progress,
                cancel,
            )
            .await?
        else {
            return Ok(None);
        };
        let own_addresses = own_addresses.to_vec();
        match query_domain_page(scope, own_addresses, offset, limit).await {
            Ok(page) => Ok(Some(page)),
            Err(error) => {
                warn!(target: "agentmail", error = %error, "domain cache query unavailable");
                Ok(None)
            }
        }
    }

    /// SQL-backed subscription ranking without retaining unsubscribe URLs or
    /// recipient tokens in the cache projection.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn top_subscriptions_page(
        &self,
        session: &mut ImapSession,
        account_name: &str,
        config: &AccountConfig,
        mailboxes: &[String],
        uid_mode: Option<u32>,
        own_addresses: &[String],
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> crate::Result<Option<CachedRankPage<CachedSubscriptionRank>>> {
        let Some(scope) = self
            .validated_rank_scope(
                session,
                account_name,
                config,
                mailboxes,
                uid_mode,
                on_progress,
                cancel,
            )
            .await?
        else {
            return Ok(None);
        };
        let own_addresses = own_addresses.to_vec();
        match query_subscription_page(scope, own_addresses, offset, limit).await {
            Ok(page) => Ok(Some(page)),
            Err(error) => {
                warn!(target: "agentmail", error = %error, "subscription cache query unavailable");
                Ok(None)
            }
        }
    }

    /// SQL-backed RFC 2919 ranking. Sender previews and pages are bounded even
    /// when a list spans a very large mailbox.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn top_mailing_lists_page(
        &self,
        session: &mut ImapSession,
        account_name: &str,
        config: &AccountConfig,
        mailboxes: &[String],
        uid_mode: Option<u32>,
        offset: usize,
        limit: usize,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> crate::Result<Option<CachedRankPage<CachedMailingListRank>>> {
        let Some(scope) = self
            .validated_rank_scope(
                session,
                account_name,
                config,
                mailboxes,
                uid_mode,
                on_progress,
                cancel,
            )
            .await?
        else {
            return Ok(None);
        };
        match query_mailing_list_page(scope, offset, limit).await {
            Ok(page) => Ok(Some(page)),
            Err(error) => {
                warn!(target: "agentmail", error = %error, "mailing-list cache query unavailable");
                Ok(None)
            }
        }
    }

    async fn validated_rank_scope(
        &self,
        session: &mut ImapSession,
        account_name: &str,
        config: &AccountConfig,
        mailboxes: &[String],
        uid_mode: Option<u32>,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> crate::Result<Option<RankScope>> {
        let Some(path) = self.path.as_ref() else {
            return Ok(None);
        };
        let account = CacheKey::new(account_name, config, "").account;
        let mut selected = Vec::with_capacity(mailboxes.len());
        let mut seen = HashSet::with_capacity(mailboxes.len());

        for mailbox in mailboxes {
            imap_client::check_cancel(cancel)?;
            let key = CacheKey::new(account_name, config, mailbox);
            if !seen.insert(key.mailbox.clone()) {
                continue;
            }
            let gate = self.gate(&key);
            let _guard = gate.lock().await;
            match self
                .sync_cached(
                    Arc::clone(path),
                    session,
                    &key,
                    uid_mode,
                    on_progress,
                    cancel,
                )
                .await
            {
                Ok(_) => selected.push(key.mailbox),
                Err(CacheSyncError::Mail(error)) => return Err(error),
                Err(CacheSyncError::Cache(CacheError::Cancelled)) => {
                    return Err(AgentmailError::Other("cancelled by client".to_string()));
                }
                Err(CacheSyncError::Cache(error)) => {
                    imap_client::check_cancel(cancel)?;
                    warn!(
                        target: "agentmail",
                        mailbox,
                        error = %error,
                        "header cache unavailable; using live ranking scan"
                    );
                    return Ok(None);
                }
            }
        }

        Ok(Some(RankScope {
            path: Arc::clone(path),
            account,
            mailboxes: selected,
        }))
    }

    /// Fence all published mailbox snapshots for an account before a mutation.
    /// Immutable header rows are retained for the next membership reconcile.
    pub(crate) async fn fence_account_mutation(&self, account_name: &str, config: &AccountConfig) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        // Do not create a cache (and account identity row) for users who have
        // never used a ranking operation. A cold sync creates the database
        // before doing network work, so an actually in-flight publisher is
        // still fenced here.
        match tokio::fs::try_exists(path.as_ref()).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                warn!(
                    target: "agentmail",
                    error = %error,
                    "could not inspect header cache before mutation"
                );
                return;
            }
        }
        let key = CacheKey::new(account_name, config, "");
        if let Err(error) = advance_account_revision(Arc::clone(path), key.account).await {
            warn!(
                target: "agentmail",
                error = %error,
                "could not fence header cache publication"
            );
        }
    }

    async fn sync_cached(
        &self,
        path: Arc<PathBuf>,
        session: &mut ImapSession,
        key: &CacheKey,
        uid_mode: Option<u32>,
        on_progress: Option<&ProgressFn>,
        cancel: Option<&CancelFn>,
    ) -> std::result::Result<StoredState, CacheSyncError> {
        let started = Instant::now();
        // Sticky across retries: once a server is known to filter
        // HEADER.FIELDS, every fetch in this sync uses full headers.
        //
        // UID Mode always starts in full-header mode. It is only ever entered on
        // Yahoo/AOL infrastructure (the UIDONLY advertisers), which strip the
        // List-Unsubscribe pair from every HEADER.FIELDS response (probe: List-Id
        // survives, List-Unsubscribe is 0 on every row even where List-Id is
        // present). So a FIELDS-first pass over this mailbox ALWAYS detects the
        // quirk and ALWAYS triggers a full-header refetch of the whole mailbox —
        // two passes over hundreds of thousands of messages. Fetching full
        // headers up front collapses that to one pass and is the only shape that
        // recovers the unsubscribe flags `top_subscriptions` ranks on.
        let mut full_header_mode = self.account_is_quirky(&key.account) || uid_mode.is_some();

        // One retry resolves a cross-process publisher or a local mutation
        // that wins the compare-and-swap while this scan is in flight.
        for _ in 0..2 {
            imap_client::check_cancel(cancel)?;
            let loaded = load_sync_state(Arc::clone(&path), key.clone()).await?;
            // The persisted quirk survives restarts; the in-process flag
            // (initialized above) covers cache-path races within a run.
            full_header_mode |= loaded.header_fields_filtered;
            let state = loaded.mailbox;
            let expected_account_revision = loaded.account_revision;
            let live_status = examine_status(session, &key.mailbox).await?;

            if let Some(state) = state
                && is_cache_hit(
                    state,
                    &live_status,
                    expected_account_revision,
                    uid_mode.is_some(),
                )
            {
                let covered = load_covered_count(Arc::clone(&path), key.clone(), state).await?;
                // Completeness yardstick. On a normal server EXISTS is the true
                // message count; in UID Mode it is only the visible-window count
                // (e.g. 1000) while the projection covers the whole mailbox, so
                // EXISTS can never equal `covered` and would force a full re-walk
                // on every warm call. Compare against the walked membership size
                // instead.
                let target = if uid_mode.is_some() {
                    load_membership_count(Arc::clone(&path), key.clone()).await?
                } else {
                    u64::from(state.exists)
                };
                let complete = projection_is_complete(uid_mode.is_some(), covered, target);
                // A complete, unpoisoned snapshot is served as-is. A projection
                // built in FIELDS mode on a HEADER.FIELDS-filtering server
                // (interrupted cold scan, or a fresh namespace after an account
                // rename) is otherwise a permanent cache hit serving
                // `has_list_headers = 0` — so fall through to the sync, which
                // heals it with a full-header refetch.
                if complete
                    && !self
                        .mailbox_projection_poisoned(&path, key, state.uid_validity)
                        .await
                {
                    imap_client::check_cancel(cancel)?;
                    if let Some(progress) = on_progress {
                        progress(target, target);
                    }
                    trace_sync(SyncKind::Hit, started, target as usize, 0, 0);
                    return Ok(state);
                }
                // A complete publication always has one marker row per member.
                // Treat a mismatch as recoverable cache damage and reconcile.
                // The 0/0 case is not damage: an EMPTY mailbox in UID Mode has
                // no membership to verify against, so it re-syncs (one cheap
                // walk of nothing) every call — routine, not warn-worthy.
                if covered == target {
                    debug!(
                        target: "agentmail",
                        mailbox = key.mailbox,
                        "empty UID-Mode mailbox re-verified (no membership to hit against)"
                    );
                } else {
                    warn!(
                        target: "agentmail",
                        mailbox = key.mailbox,
                        expected = target,
                        actual = covered,
                        "header cache snapshot was incomplete; reconciling"
                    );
                }
            }

            let Some(uid_validity) = live_status.uid_validity else {
                return Err(CacheError::Invariant(format!(
                    "mailbox {:?} omitted UIDVALIDITY",
                    key.mailbox
                ))
                .into());
            };
            if uid_validity == 0 {
                return Err(CacheError::Invariant(format!(
                    "mailbox {:?} returned UIDVALIDITY 0",
                    key.mailbox
                ))
                .into());
            }

            let expected_revision = loaded.existing_revision;
            let regression = state.is_some_and(|value| {
                value.uid_validity == uid_validity
                    && matches!((value.uid_next, live_status.uid_next), (Some(old), Some(new)) if new < old)
            });

            let tail_base = state.and_then(|value| match (value.uid_next, live_status.uid_next) {
                (Some(old), Some(new)) if value.uid_validity == uid_validity && new > old => {
                    Some((value, old))
                }
                _ => None,
            });

            let (snapshot, mut live_uids, sync_kind, snapshot_stable) = if let Some(message_limit) =
                uid_mode
            {
                // UID Mode: walk the WHOLE mailbox past the visible window
                // (SEARCH would only see the window). Always a full
                // membership pass — the header fetch below still skips
                // already-cached UIDs, so only new headers are fetched.
                let membership =
                    imap_client::walk_all_uids_uidmode(session, message_limit, on_progress, cancel)
                        .await?;
                let snapshot = examine_status(session, &key.mailbox).await?;
                let kind = if state.is_none()
                    || state.is_some_and(|value| value.uid_validity != uid_validity)
                {
                    SyncKind::Cold
                } else {
                    SyncKind::Membership
                };
                (snapshot, membership, kind, true)
            } else if let Some((base, from_uid)) = tail_base {
                let (snapshot, tail, tail_stable) =
                    stable_uid_search(session, &key.mailbox, SearchScope::Tail(from_uid), cancel)
                        .await?;
                let membership = load_membership(Arc::clone(&path), key.clone()).await?;
                if tail_stable && pure_append_is_proven(base, &snapshot, &membership, &tail) {
                    let mut combined = membership;
                    combined.extend(tail);
                    combined.sort_unstable();
                    combined.dedup();
                    (snapshot, combined, SyncKind::Tail, true)
                } else {
                    let (snapshot, membership, stable) =
                        stable_uid_search(session, &key.mailbox, SearchScope::All, cancel).await?;
                    (snapshot, membership, SyncKind::Membership, stable)
                }
            } else {
                let kind = if state.is_none()
                    || state.is_some_and(|value| value.uid_validity != uid_validity)
                    || regression
                {
                    SyncKind::Cold
                } else {
                    SyncKind::Membership
                };
                let (snapshot, membership, stable) =
                    stable_uid_search(session, &key.mailbox, SearchScope::All, cancel).await?;
                (snapshot, membership, kind, stable)
            };

            let Some(snapshot_uid_validity) = snapshot.uid_validity else {
                return Err(CacheError::Invariant(format!(
                    "mailbox {:?} omitted UIDVALIDITY while reconciling",
                    key.mailbox
                ))
                .into());
            };

            let snapshot_regression = state.is_some_and(|value| {
                value.uid_validity == snapshot_uid_validity
                    && matches!((value.uid_next, snapshot.uid_next), (Some(old), Some(new)) if new < old)
            });
            // Chunk commits from an interrupted cold scan are reusable too:
            // the immutable identity key includes the newly observed
            // UIDVALIDITY even before a mailbox_state row is published.
            let reuse_existing = !regression && !snapshot_regression;
            let known_uids = if reuse_existing {
                load_header_uids(Arc::clone(&path), key.clone(), snapshot_uid_validity).await?
            } else {
                HashSet::new()
            };

            let mut missing: Vec<u32> = live_uids
                .iter()
                .copied()
                .filter(|uid| !known_uids.contains(uid))
                .collect();
            missing.sort_unstable();
            let reused = live_uids.len().saturating_sub(missing.len());
            if let Some(progress) = on_progress {
                progress(reused as u64, live_uids.len() as u64);
            }

            // Did we begin in full-header mode because the account was
            // already flagged quirky? If so, this sync's fresh-batch
            // detection can't fire (it guards on `!full_header_mode`), so a
            // projection poisoned by an earlier FIELDS-mode build would never
            // heal without the stored-state check below.
            let started_quirky = full_header_mode;

            let mut processed = 0usize;
            let mut list_id_rows = 0usize;
            let mut unsubscribe_rows = 0usize;
            for chunk in missing.chunks(FETCH_CHUNK_SIZE) {
                imap_client::check_cancel(cancel)?;
                let rows = if full_header_mode {
                    imap_client::fetch_rank_headers_full_for_uids(session, chunk, None, cancel)
                        .await?
                } else {
                    imap_client::fetch_rank_headers_for_uids(session, chunk, None, cancel).await?
                };
                let mut rows = rows;
                for row in &mut rows {
                    row.uid_validity = Some(snapshot_uid_validity);
                }
                if !full_header_mode {
                    list_id_rows += rows.iter().filter(|row| row.list_id.is_some()).count();
                    unsubscribe_rows += rows
                        .iter()
                        .filter(|row| {
                            row.list_unsubscribe.is_some() || row.list_unsubscribe_post.is_some()
                        })
                        .count();
                }
                reconcile_fetch_chunk(&key.mailbox, chunk, &rows, &mut live_uids)?;
                store_headers(Arc::clone(&path), key.clone(), snapshot_uid_validity, rows).await?;
                processed += chunk.len();
                if let Some(progress) = on_progress {
                    progress(
                        (reused + processed).min(reused + missing.len()) as u64,
                        (reused + missing.len()) as u64,
                    );
                }
            }

            // Server-quirk fallback (AOL/Yahoo): bulk mail is clearly present
            // (List-Id parsed on many rows) yet not one fetched row carried a
            // List-Unsubscribe header — the server filtered the pair out of
            // its HEADER.FIELDS responses. Refetch the whole membership with
            // full headers so the flags are derived from ground truth, and
            // remember the account so future syncs skip the wasted pass.
            // (Detection sees only rows fetched THIS sync; a scan resumed
            // from an interrupted pre-detection run may need one more full
            // Membership sync to converge.)
            // Heal a HEADER.FIELDS-filtered projection with a full-header
            // refetch of the whole membership. Two triggers:
            //  - fresh detection: this sync fetched many List-Id rows in
            //    FIELDS mode and saw zero unsubscribe headers; or
            //  - stored poisoning: the account is already known quirky, but
            //    the projection on disk was built (partly) in FIELDS mode and
            //    still shows the List-Id-without-unsubscribe signature —
            //    incremental syncs alone never re-fetch those rows.
            let fresh_detection = !full_header_mode
                && imap_client::header_fields_quirk(list_id_rows, unsubscribe_rows);
            let stored_poisoned = started_quirky
                && self
                    .mailbox_projection_poisoned(&path, key, snapshot_uid_validity)
                    .await;
            if fresh_detection || stored_poisoned {
                warn!(
                    target: "agentmail",
                    mailbox = key.mailbox,
                    list_id_rows,
                    stored_poisoned,
                    "healing HEADER.FIELDS-filtered projection with a full-header refetch of the whole mailbox",
                );
                self.persist_account_quirky(&key.account).await;
                full_header_mode = true;
                // Buffer the whole refetch and store it in one shot: a heal
                // interrupted midway (this server drops long connections) must
                // leave the projection FULLY poisoned so it re-heals next
                // time. A partial write would set some `has_list_headers = 1`,
                // making the poison check read "clean" and stranding the rest
                // at 0 forever.
                let members: Vec<u32> = live_uids.clone();
                let mut healed: Vec<ListHeaderRow> = Vec::with_capacity(members.len());
                for chunk in members.chunks(FETCH_CHUNK_SIZE) {
                    imap_client::check_cancel(cancel)?;
                    let mut rows =
                        imap_client::fetch_rank_headers_full_for_uids(session, chunk, None, cancel)
                            .await?;
                    for row in &mut rows {
                        row.uid_validity = Some(snapshot_uid_validity);
                    }
                    reconcile_fetch_chunk(&key.mailbox, chunk, &rows, &mut live_uids)?;
                    healed.extend(rows);
                }
                store_headers(
                    Arc::clone(&path),
                    key.clone(),
                    snapshot_uid_validity,
                    healed,
                )
                .await?;
            }

            imap_client::check_cancel(cancel)?;
            // Header fetching can take a long time on a cold 216K-message
            // mailbox. UIDNEXT/count may advance during that work, but cached
            // immutable headers remain valid only while the UIDVALIDITY epoch
            // itself is unchanged.
            let final_status = examine_status(session, &key.mailbox).await?;
            if final_status.uid_validity != Some(snapshot_uid_validity) {
                continue;
            }
            let outcome = publish_snapshot(
                Arc::clone(&path),
                key.clone(),
                expected_revision,
                expected_account_revision,
                snapshot_uid_validity,
                snapshot_stable.then_some(snapshot.uid_next).flatten(),
                &live_uids,
                cancel.cloned(),
            )
            .await?;
            if outcome == PublishOutcome::Conflict {
                continue;
            }

            let published_state = load_state(Arc::clone(&path), key.clone())
                .await?
                .ok_or_else(|| CacheError::Invariant("published state disappeared".to_string()))?;
            let covered =
                load_covered_count(Arc::clone(&path), key.clone(), published_state).await?;
            if covered != live_uids.len() as u64 {
                return Err(CacheError::Invariant(format!(
                    "published {} members but covered {covered} rows",
                    live_uids.len(),
                ))
                .into());
            }
            trace_sync(
                sync_kind,
                started,
                live_uids.len(),
                missing.len(),
                state
                    .map_or(0, |value| value.exists as usize)
                    .saturating_sub(live_uids.len()),
            );
            return Ok(published_state);
        }

        Err(CacheError::Conflict.into())
    }
}

#[derive(Debug, Clone, Copy)]
enum SearchScope {
    All,
    Tail(u32),
}

/// Reconcile a header-fetch chunk against the UIDs requested, dropping any
/// the server omitted from `live_uids`.
///
/// A few omissions are normal (a UID expunged between SEARCH and FETCH). But
/// an **entire** non-empty chunk coming back empty is the signature of a
/// server rejection that async-imap's FETCH parser swallows into an empty
/// `Ok` (its `take_while` stops at the tagged `Done` without reading NO/BAD).
/// Silently pruning a whole chunk would corrupt the projection, so that case
/// is surfaced as an error instead — the scan's resume loop then retries on a
/// fresh session.
fn reconcile_fetch_chunk(
    mailbox: &str,
    chunk: &[u32],
    rows: &[ListHeaderRow],
    live_uids: &mut Vec<u32>,
) -> crate::Result<()> {
    if rows.is_empty() && !chunk.is_empty() {
        return Err(AgentmailError::Other(format!(
            "mailbox {mailbox:?} returned no rows for a {}-UID header fetch (likely a swallowed server rejection); aborting to avoid corrupting the cache",
            chunk.len()
        )));
    }
    let returned: HashSet<u32> = rows.iter().map(|row| row.uid).collect();
    let omitted: HashSet<u32> = chunk
        .iter()
        .copied()
        .filter(|uid| !returned.contains(uid))
        .collect();
    if !omitted.is_empty() {
        live_uids.retain(|uid| !omitted.contains(uid));
    }
    Ok(())
}

async fn stable_uid_search(
    session: &mut ImapSession,
    mailbox: &str,
    scope: SearchScope,
    cancel: Option<&CancelFn>,
) -> crate::Result<(MailboxStatus, Vec<u32>, bool)> {
    let mut same_epoch_fallback = None;
    for _ in 0..MAX_STABLE_SEARCH_ATTEMPTS {
        imap_client::check_cancel(cancel)?;
        let before = examine_status(session, mailbox).await?;
        let mut uids = match scope {
            // Truncation-guarded: a short/empty result from a MESSAGELIMIT
            // server is rediscovered in bounded windows instead of being
            // published as an empty membership.
            SearchScope::All => {
                imap_client::search_all_uids_checked(session, before.exists, before.uid_next)
                    .await?
            }
            SearchScope::Tail(from_uid) => imap_client::search_uids_from(session, from_uid).await?,
        };
        uids.sort_unstable();
        uids.dedup();
        let after = examine_status(session, mailbox).await?;
        if same_snapshot(&before, &after) {
            return Ok((after, uids, true));
        }
        if before.uid_validity.is_some() && before.uid_validity == after.uid_validity {
            same_epoch_fallback = Some((after, uids, false));
        }
    }

    if let Some(snapshot) = same_epoch_fallback {
        debug!(
            target: "agentmail",
            mailbox,
            "mailbox remained busy during UID search; publishing a reconcile-required snapshot"
        );
        Ok(snapshot)
    } else {
        Err(AgentmailError::Other(format!(
            "mailbox {mailbox:?} changed UIDVALIDITY continuously while synchronizing"
        )))
    }
}

async fn examine_status(session: &mut ImapSession, mailbox: &str) -> crate::Result<MailboxStatus> {
    let selected = imap_client::examine(session, mailbox).await?;
    Ok(MailboxStatus {
        uid_validity: selected.uid_validity,
        uid_next: selected.uid_next,
        exists: selected.exists,
        highest_modseq: None,
    })
}

fn same_snapshot(left: &MailboxStatus, right: &MailboxStatus) -> bool {
    left.uid_validity == right.uid_validity
        && left.uid_next == right.uid_next
        && left.exists == right.exists
}

/// Whether a stored snapshot may be served without a resync. Beyond the
/// mailbox triple, the snapshot must predate no local mutation: Yahoo/AOL pin
/// INBOX `EXISTS` to a sliding window (older mail backfills the view), so a
/// delete can leave the triple identical while the ranking data changed.
///
/// In UID Mode the stored `message_count` is the FULL mailbox (from the
/// PARTIAL walk), while `EXISTS` still reports only the visible window — so
/// the `EXISTS` comparison is skipped there and freshness rests on
/// UIDVALIDITY + UIDNEXT + the mutation fence (all of which are full-mailbox
/// accurate in UID Mode).
fn is_cache_hit(
    state: StoredState,
    status: &MailboxStatus,
    account_revision: i64,
    uid_mode: bool,
) -> bool {
    status.uid_validity == Some(state.uid_validity)
        && state.uid_next.is_some()
        && status.uid_next == state.uid_next
        && (uid_mode || status.exists == state.exists)
        && state.account_revision == account_revision
}

/// Whether a projection covers its whole mailbox and can be served without a
/// resync. `target` is the number of messages that must each own a projected
/// header row: the walked membership size in UID Mode (EXISTS there is only the
/// visible-window count and can never equal full coverage), or EXISTS in
/// Limited Mode. A UID-Mode `target` of 0 means the mailbox was never walked
/// and must sync rather than serve an empty hit; a Limited-Mode `target` of 0
/// is a legitimately empty mailbox that hits.
fn projection_is_complete(uid_mode: bool, covered: u64, target: u64) -> bool {
    if uid_mode {
        target > 0 && covered == target
    } else {
        covered == target
    }
}

fn pure_append_is_proven(
    state: StoredState,
    status: &MailboxStatus,
    membership: &[u32],
    tail: &[u32],
) -> bool {
    let Some(old_uid_next) = state.uid_next else {
        return false;
    };
    let Some(new_uid_next) = status.uid_next else {
        return false;
    };
    status.uid_validity == Some(state.uid_validity)
        && new_uid_next >= old_uid_next
        && membership.len() == state.exists as usize
        && strictly_increasing(membership)
        && membership.iter().all(|uid| *uid < old_uid_next)
        && strictly_increasing(tail)
        && tail.iter().all(|uid| *uid >= old_uid_next)
        && u32::try_from(tail.len())
            .ok()
            .and_then(|tail_count| state.exists.checked_add(tail_count))
            .is_some_and(|expected| expected == status.exists)
}

fn strictly_increasing(values: &[u32]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn trace_sync(
    kind: SyncKind,
    started: Instant,
    result_count: usize,
    fetched_count: usize,
    pruned_count: usize,
) {
    debug!(
        target: "agentmail",
        operation = "rank_header_sync",
        cache = kind.as_str(),
        elapsed_ms = started.elapsed().as_millis(),
        result_count,
        fetched_count,
        pruned_count,
        "validated header cache sync complete"
    );
}

fn normalize_mailbox(mailbox: &str) -> String {
    if mailbox.eq_ignore_ascii_case("INBOX") {
        "INBOX".to_string()
    } else {
        mailbox.to_string()
    }
}

/// Return the RFC 2919 identifier and a display label without retaining the
/// complete header field. Invalid or ambiguous List-Id values are not cached.
fn normalized_list_id_fields(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let start = value.find('<')?;
    let end = value[start + 1..].find('>')? + start + 1;
    if !value[end + 1..].trim().is_empty()
        || value[..start].contains(['<', '>'])
        || value[start + 1..end].contains(['<', '>', ' ', '\t', '\r', '\n'])
    {
        return None;
    }
    let identifier = value[start + 1..end].trim();
    if identifier.is_empty() {
        return None;
    }
    let identifier = identifier.to_ascii_lowercase();
    let display = value[..start].trim();
    let display = if display.is_empty() {
        identifier.clone()
    } else {
        display.to_string()
    };
    Some((identifier, display))
}

fn default_cache_path() -> Option<PathBuf> {
    if std::env::var("AGENTMAIL_DISABLE_HEADER_CACHE")
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    {
        return None;
    }
    let root = std::env::var_os("AGENTMAIL_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::cache_dir)?;
    Some(root.join("agentmail").join(HeaderCache::FILE_NAME))
}

fn open_connection(path: &Path) -> CacheResult<Connection> {
    if let Some(parent) = path.parent() {
        let parent_existed = parent.exists();
        std::fs::create_dir_all(parent)?;
        // The environment override and builder may point at a caller-owned
        // shared directory. Never chmod an existing parent such as `/tmp`;
        // only directories created specifically for this cache are tightened.
        if !parent_existed {
            restrict_directory(parent)?;
        }
    }
    let mut connection = Connection::open(path)?;
    // Tighten an existing legacy file before reading or rebuilding any
    // token-bearing projection it may contain.
    restrict_file(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", true)?;
    migrate_schema(&mut connection)?;
    // Lazily ensured outside schema versioning so adding it does not force a
    // projection rebuild on existing caches: server-behavior quirks observed
    // by scans (e.g. AOL/Yahoo filtering List-* from HEADER.FIELDS), keyed
    // like account_state. Persisted so a process restart cannot silently
    // drop back to a fetch mode the server is known to corrupt.
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_quirks (
             account_key TEXT NOT NULL PRIMARY KEY,
             header_fields_filtered INTEGER NOT NULL DEFAULT 0
         ) WITHOUT ROWID;",
    )?;
    Ok(connection)
}

fn migrate_schema(connection: &mut Connection) -> CacheResult<()> {
    let observed: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if observed == CACHE_SCHEMA_VERSION {
        return Ok(());
    }

    // Destructive legacy upgrades enable secure deletion below. The additive
    // v5/v6 -> v7 migration returns before any table rebuild or VACUUM.
    connection.pragma_update(None, "secure_delete", true)?;

    // Serialize the version check with the migration itself. Without the
    // immediate transaction, two fresh MCP calls could both observe an old
    // version and the second migrator could drop the first one's new tables.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == CACHE_SCHEMA_VERSION {
        transaction.commit()?;
        return Ok(());
    }
    if version == 5 || version == 6 {
        if version == 5 {
            transaction.execute(
                "ALTER TABLE header_rows
                     ADD COLUMN sender_domain TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        let sender_addresses = {
            let mut statement =
                transaction.prepare("SELECT DISTINCT sender_email FROM header_rows")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        transaction.execute_batch(
            "CREATE TEMP TABLE domain_backfill (
                 old_sender_email TEXT NOT NULL PRIMARY KEY,
                 canonical_sender_email TEXT NOT NULL,
                 sender_domain TEXT NOT NULL
             ) WITHOUT ROWID;",
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO domain_backfill (
                     old_sender_email, canonical_sender_email, sender_domain
                 ) VALUES (?1, ?2, ?3)",
            )?;
            for address in sender_addresses {
                let canonical =
                    crate::config::canonicalize_email(&address).unwrap_or_else(|| address.clone());
                let domain = domain_from_address(&canonical).unwrap_or_default();
                insert.execute(params![address, canonical, domain])?;
            }
        }
        transaction.execute_batch(
            "UPDATE header_rows
                SET sender_email = COALESCE(
                        (SELECT backfill.canonical_sender_email
                           FROM domain_backfill AS backfill
                          WHERE backfill.old_sender_email = header_rows.sender_email),
                        sender_email
                    ),
                    sender_domain = COALESCE(
                    (SELECT backfill.sender_domain
                       FROM domain_backfill AS backfill
                      WHERE backfill.old_sender_email = header_rows.sender_email),
                    ''
                );
             DROP TABLE domain_backfill;
             CREATE INDEX IF NOT EXISTS header_rows_by_domain
                 ON header_rows (
                     account_key, mailbox, uid_validity, projection_version,
                     sender_domain, uid
                 );",
        )?;
        transaction.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION)?;
        transaction.commit()?;
        return Ok(());
    }

    // Older versions replace this disposable projection. Secure deletion,
    // VACUUM, and a truncated WAL ensure obsolete token-bearing columns from
    // pre-v3 databases are not left in SQLite free pages or the WAL file.
    transaction.execute_batch(
        "DROP TABLE IF EXISTS membership;
         DROP TABLE IF EXISTS header_rows;
         DROP TABLE IF EXISTS mailbox_state;
         DROP TABLE IF EXISTS account_state;
         DROP TABLE IF EXISTS account_quirks;",
    )?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_state (
             account_key TEXT NOT NULL PRIMARY KEY,
             mutation_revision INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS mailbox_state (
             account_key TEXT NOT NULL,
             mailbox TEXT NOT NULL,
             uid_validity INTEGER NOT NULL,
             uid_next INTEGER,
             message_count INTEGER NOT NULL,
             revision INTEGER NOT NULL,
             projection_version INTEGER NOT NULL,
             account_revision INTEGER NOT NULL,
             PRIMARY KEY (account_key, mailbox)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS membership (
             account_key TEXT NOT NULL,
             mailbox TEXT NOT NULL,
             uid INTEGER NOT NULL,
             PRIMARY KEY (account_key, mailbox, uid)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS header_rows (
             account_key TEXT NOT NULL,
             mailbox TEXT NOT NULL,
             uid_validity INTEGER NOT NULL,
             uid INTEGER NOT NULL,
             projection_version INTEGER NOT NULL,
             sender_email TEXT NOT NULL,
             sender_domain TEXT NOT NULL,
             sender_name TEXT NOT NULL,
             date_unix_ms INTEGER,
             message_id TEXT,
             list_id TEXT,
             list_display_name TEXT,
             has_list_headers INTEGER NOT NULL CHECK (has_list_headers IN (0, 1)),
             advertised_one_click INTEGER NOT NULL CHECK (advertised_one_click IN (0, 1)),
             PRIMARY KEY (
                 account_key, mailbox, uid_validity, uid, projection_version
             )
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS header_rows_by_domain
             ON header_rows (
                 account_key, mailbox, uid_validity, projection_version,
                 sender_domain, uid
             );",
    )?;
    transaction.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION)?;
    transaction.commit()?;
    connection.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> CacheResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> CacheResult<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> CacheResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> CacheResult<()> {
    Ok(())
}

fn prepare_rank_connection(scope: &RankScope, own_addresses: &[String]) -> CacheResult<Connection> {
    let mut connection = open_connection(&scope.path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TEMP TABLE selected_scope (
             account_key TEXT NOT NULL,
             projection_version INTEGER NOT NULL
         );
         CREATE TEMP TABLE selected_mailboxes (
             mailbox TEXT NOT NULL PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE own_addresses (
             address TEXT NOT NULL PRIMARY KEY
         ) WITHOUT ROWID;",
    )?;
    transaction.execute(
        "INSERT INTO selected_scope (account_key, projection_version) VALUES (?1, ?2)",
        params![scope.account, HEADER_PROJECTION_VERSION],
    )?;
    {
        let mut insert = transaction
            .prepare("INSERT OR IGNORE INTO selected_mailboxes (mailbox) VALUES (?1)")?;
        for mailbox in &scope.mailboxes {
            insert.execute(params![mailbox])?;
        }
    }
    {
        let mut insert =
            transaction.prepare("INSERT OR IGNORE INTO own_addresses (address) VALUES (?1)")?;
        for address in own_addresses {
            insert.execute(params![address])?;
        }
    }
    transaction.commit()?;

    // A non-empty Message-ID is account-wide identity. Missing IDs partition
    // by mailbox epoch and UID so unrelated messages are never collapsed.
    // The representative ordering mirrors the live ranking tie-breaker while
    // remaining deterministic across mailbox scan-order changes.
    connection.execute_batch(
        "CREATE TEMP VIEW rank_rows AS
         SELECT mailbox, uid_validity, uid, sender_email, sender_domain,
                sender_name, date_unix_ms, message_id, list_id, list_display_name,
                has_list_headers, advertised_one_click
           FROM (
             SELECT h.mailbox, h.uid_validity, h.uid, h.sender_email,
                    h.sender_domain, h.sender_name, h.date_unix_ms,
                    h.message_id, h.list_id, h.list_display_name,
                    h.has_list_headers,
                    h.advertised_one_click,
                    ROW_NUMBER() OVER (
                        PARTITION BY
                          CASE WHEN NULLIF(TRIM(h.message_id), '') IS NULL
                               THEN 1 ELSE 0 END,
                          CASE WHEN NULLIF(TRIM(h.message_id), '') IS NULL
                               THEN h.mailbox ELSE TRIM(h.message_id) END,
                          CASE WHEN NULLIF(TRIM(h.message_id), '') IS NULL
                               THEN h.uid_validity ELSE 0 END,
                          CASE WHEN NULLIF(TRIM(h.message_id), '') IS NULL
                               THEN h.uid ELSE 0 END
                        ORDER BY (h.date_unix_ms IS NOT NULL) DESC,
                                 h.date_unix_ms DESC, h.mailbox DESC, h.uid DESC
                    ) AS duplicate_rank
               FROM selected_scope AS scope
               JOIN selected_mailboxes AS selected
               JOIN mailbox_state AS state
                 ON state.account_key = scope.account_key
                AND state.mailbox = selected.mailbox
                AND state.projection_version = scope.projection_version
               JOIN membership AS membership
                 ON membership.account_key = state.account_key
                AND membership.mailbox = state.mailbox
               JOIN header_rows AS h
                 ON h.account_key = membership.account_key
                AND h.mailbox = membership.mailbox
                AND h.uid = membership.uid
                AND h.uid_validity = state.uid_validity
                AND h.projection_version = scope.projection_version
           ) AS candidates
          WHERE duplicate_rank = 1;",
    )?;
    Ok(connection)
}

async fn query_sender_page(
    scope: RankScope,
    own_addresses: Vec<String>,
    offset: usize,
    limit: usize,
) -> CacheResult<CachedRankPage<CachedSenderRank>> {
    tokio::task::spawn_blocking(move || {
        let connection = prepare_rank_connection(&scope, &own_addresses)?;
        connection.execute_batch(
            "CREATE TEMP VIEW sender_rank_groups AS
             WITH eligible AS (
                 SELECT * FROM rank_rows AS row
                  WHERE row.sender_email != ''
                    AND NOT EXISTS (
                        SELECT 1 FROM own_addresses AS own
                         WHERE own.address = row.sender_email
                    )
             ),
             grouped AS (
                 SELECT sender_email, sender_name, COUNT(*) AS message_count,
                        MIN(date_unix_ms) AS oldest_date_unix_ms,
                        MAX(date_unix_ms) AS newest_date_unix_ms
                   FROM eligible
                  GROUP BY sender_email, sender_name
             ),
             samples AS (
                 SELECT sender_email, sender_name, mailbox, uid_validity, uid,
                        date_unix_ms,
                        ROW_NUMBER() OVER (
                            PARTITION BY sender_email, sender_name
                            ORDER BY (date_unix_ms IS NOT NULL) DESC,
                                     date_unix_ms DESC, mailbox DESC, uid DESC
                        ) AS sample_rank
                   FROM eligible
             )
             SELECT grouped.sender_email, grouped.sender_name,
                    grouped.message_count, grouped.oldest_date_unix_ms,
                    grouped.newest_date_unix_ms, samples.mailbox,
                    samples.uid_validity, samples.uid, samples.date_unix_ms
               FROM grouped
               JOIN samples
                 ON samples.sender_email = grouped.sender_email
                AND samples.sender_name = grouped.sender_name
                AND samples.sample_rank = 1;",
        )?;
        let (total_groups, total_messages) = rank_totals(&connection, "sender_rank_groups")?;
        let mut statement = connection.prepare(
            "SELECT sender_email, sender_name, message_count,
                    oldest_date_unix_ms, newest_date_unix_ms, mailbox,
                    uid_validity, uid, date_unix_ms
               FROM sender_rank_groups
              ORDER BY message_count DESC, sender_email, sender_name
              LIMIT ?1 OFFSET ?2",
        )?;
        let rows =
            statement.query_map(params![sql_rank_limit(limit)?, sql_offset(offset)], |row| {
                Ok(CachedSenderRank {
                    address: row.get(0)?,
                    display_name: row.get(1)?,
                    count: sql_u64(row.get::<_, i64>(2)?)?,
                    oldest_date: sql_date(row.get(3)?),
                    newest_date: sql_date(row.get(4)?),
                    sample: CachedRankSample {
                        mailbox: row.get(5)?,
                        uid_validity: sql_u32(row.get::<_, i64>(6)?)?,
                        uid: sql_u32(row.get::<_, i64>(7)?)?,
                        date: sql_date(row.get(8)?),
                    },
                })
            })?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(CachedRankPage {
            total_messages,
            total_groups,
            items,
        })
    })
    .await?
}

async fn query_domain_page(
    scope: RankScope,
    own_addresses: Vec<String>,
    offset: usize,
    limit: usize,
) -> CacheResult<CachedRankPage<CachedDomainRank>> {
    tokio::task::spawn_blocking(move || {
        let connection = prepare_rank_connection(&scope, &own_addresses)?;
        connection.execute_batch(
            "CREATE TEMP VIEW domain_rank_groups AS
             WITH eligible AS (
                 SELECT * FROM rank_rows AS row
                  WHERE row.sender_domain != ''
                    AND row.sender_email != ''
                    AND NOT EXISTS (
                        SELECT 1 FROM own_addresses AS own
                         WHERE own.address = row.sender_email
                    )
             ),
             grouped AS (
                 SELECT sender_domain, COUNT(*) AS message_count,
                        MIN(date_unix_ms) AS oldest_date_unix_ms,
                        MAX(date_unix_ms) AS newest_date_unix_ms
                   FROM eligible
                  GROUP BY sender_domain
             ),
             samples AS (
                 SELECT sender_domain, mailbox, uid_validity, uid, date_unix_ms,
                        ROW_NUMBER() OVER (
                            PARTITION BY sender_domain
                            ORDER BY (date_unix_ms IS NOT NULL) DESC,
                                     date_unix_ms DESC, mailbox DESC, uid DESC
                        ) AS sample_rank
                   FROM eligible
             )
             SELECT grouped.sender_domain, grouped.message_count,
                    grouped.oldest_date_unix_ms,
                    grouped.newest_date_unix_ms, samples.mailbox,
                    samples.uid_validity, samples.uid, samples.date_unix_ms
               FROM grouped
               JOIN samples
                 ON samples.sender_domain = grouped.sender_domain
                AND samples.sample_rank = 1;",
        )?;
        let (total_groups, total_messages) = rank_totals(&connection, "domain_rank_groups")?;
        let mut statement = connection.prepare(
            "SELECT sender_domain, message_count, oldest_date_unix_ms,
                    newest_date_unix_ms, mailbox, uid_validity, uid,
                    date_unix_ms
               FROM domain_rank_groups
              ORDER BY message_count DESC, sender_domain
              LIMIT ?1 OFFSET ?2",
        )?;
        let rows =
            statement.query_map(params![sql_rank_limit(limit)?, sql_offset(offset)], |row| {
                Ok(CachedDomainRank {
                    domain: row.get(0)?,
                    count: sql_u64(row.get::<_, i64>(1)?)?,
                    oldest_date: sql_date(row.get(2)?),
                    newest_date: sql_date(row.get(3)?),
                    sample: CachedRankSample {
                        mailbox: row.get(4)?,
                        uid_validity: sql_u32(row.get::<_, i64>(5)?)?,
                        uid: sql_u32(row.get::<_, i64>(6)?)?,
                        date: sql_date(row.get(7)?),
                    },
                })
            })?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(CachedRankPage {
            total_messages,
            total_groups,
            items,
        })
    })
    .await?
}

async fn query_subscription_page(
    scope: RankScope,
    own_addresses: Vec<String>,
    offset: usize,
    limit: usize,
) -> CacheResult<CachedRankPage<CachedSubscriptionRank>> {
    tokio::task::spawn_blocking(move || {
        let connection = prepare_rank_connection(&scope, &own_addresses)?;
        connection.execute_batch(
            "CREATE TEMP VIEW subscription_rank_groups AS
             WITH eligible AS (
                 SELECT * FROM rank_rows AS row
                  WHERE row.has_list_headers = 1
                    AND row.sender_email != ''
                    AND NOT EXISTS (
                        SELECT 1 FROM own_addresses AS own
                         WHERE own.address = row.sender_email
                    )
             ),
             grouped AS (
                 SELECT sender_email, COUNT(*) AS message_count,
                        MIN(date_unix_ms) AS oldest_date_unix_ms,
                        MAX(date_unix_ms) AS newest_date_unix_ms
                   FROM eligible
                  GROUP BY sender_email
             ),
             samples AS (
                 SELECT sender_email, mailbox, uid_validity, uid,
                        date_unix_ms, advertised_one_click,
                        ROW_NUMBER() OVER (
                            PARTITION BY sender_email
                            ORDER BY (date_unix_ms IS NOT NULL) DESC,
                                     date_unix_ms DESC, mailbox DESC, uid DESC
                        ) AS sample_rank
                   FROM eligible
             )
             SELECT grouped.sender_email, grouped.message_count,
                    grouped.oldest_date_unix_ms,
                    grouped.newest_date_unix_ms, samples.mailbox,
                    samples.uid_validity, samples.uid, samples.date_unix_ms,
                    samples.advertised_one_click
               FROM grouped
               JOIN samples
                 ON samples.sender_email = grouped.sender_email
                AND samples.sample_rank = 1;",
        )?;
        let (total_groups, total_messages) = rank_totals(&connection, "subscription_rank_groups")?;
        let mut statement = connection.prepare(
            "SELECT sender_email, message_count,
                    oldest_date_unix_ms, newest_date_unix_ms, mailbox,
                    uid_validity, uid, date_unix_ms, advertised_one_click
               FROM subscription_rank_groups
              ORDER BY advertised_one_click DESC, message_count DESC,
                       sender_email
              LIMIT ?1 OFFSET ?2",
        )?;
        let rows =
            statement.query_map(params![sql_rank_limit(limit)?, sql_offset(offset)], |row| {
                Ok(CachedSubscriptionRank {
                    address: row.get(0)?,
                    count: sql_u64(row.get::<_, i64>(1)?)?,
                    oldest_date: sql_date(row.get(2)?),
                    newest_date: sql_date(row.get(3)?),
                    sample: CachedRankSample {
                        mailbox: row.get(4)?,
                        uid_validity: sql_u32(row.get::<_, i64>(5)?)?,
                        uid: sql_u32(row.get::<_, i64>(6)?)?,
                        date: sql_date(row.get(7)?),
                    },
                    advertised_one_click: row.get::<_, i64>(8)? != 0,
                })
            })?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(CachedRankPage {
            total_messages,
            total_groups,
            items,
        })
    })
    .await?
}

async fn query_mailing_list_page(
    scope: RankScope,
    offset: usize,
    limit: usize,
) -> CacheResult<CachedRankPage<CachedMailingListRank>> {
    tokio::task::spawn_blocking(move || {
        let mut connection = prepare_rank_connection(&scope, &[])?;
        connection.execute_batch(
            "CREATE TEMP VIEW mailing_list_rank_groups AS
             WITH eligible AS (
                 SELECT * FROM rank_rows WHERE list_id IS NOT NULL
             ),
             grouped AS (
                 SELECT list_id, COUNT(*) AS message_count,
                        COUNT(DISTINCT NULLIF(sender_email, '')) AS sender_count,
                        MIN(date_unix_ms) AS oldest_date_unix_ms,
                        MAX(date_unix_ms) AS newest_date_unix_ms
                   FROM eligible
                  GROUP BY list_id
             ),
             samples AS (
                 SELECT list_id, list_display_name, mailbox, uid_validity, uid,
                        date_unix_ms,
                        ROW_NUMBER() OVER (
                            PARTITION BY list_id
                            ORDER BY (date_unix_ms IS NOT NULL) DESC,
                                     date_unix_ms DESC, mailbox DESC, uid DESC
                        ) AS sample_rank
                   FROM eligible
             )
             SELECT grouped.list_id,
                    COALESCE(NULLIF(samples.list_display_name, ''), grouped.list_id)
                        AS list_display_name,
                    grouped.message_count, grouped.sender_count,
                    grouped.oldest_date_unix_ms, grouped.newest_date_unix_ms,
                    samples.mailbox, samples.uid_validity, samples.uid,
                    samples.date_unix_ms
               FROM grouped
               JOIN samples
                 ON samples.list_id = grouped.list_id
                AND samples.sample_rank = 1;",
        )?;
        let (total_groups, total_messages) = rank_totals(&connection, "mailing_list_rank_groups")?;
        let mut statement = connection.prepare(
            "SELECT list_id, list_display_name, message_count, sender_count,
                    oldest_date_unix_ms, newest_date_unix_ms, mailbox,
                    uid_validity, uid, date_unix_ms
               FROM mailing_list_rank_groups
              ORDER BY message_count DESC, list_id
              LIMIT ?1 OFFSET ?2",
        )?;
        let rows =
            statement.query_map(params![sql_rank_limit(limit)?, sql_offset(offset)], |row| {
                Ok(CachedMailingListRank {
                    list_id: row.get(0)?,
                    display_name: row.get(1)?,
                    count: sql_u64(row.get::<_, i64>(2)?)?,
                    sender_count: sql_u64(row.get::<_, i64>(3)?)?,
                    oldest_date: sql_date(row.get(4)?),
                    newest_date: sql_date(row.get(5)?),
                    sample: CachedRankSample {
                        mailbox: row.get(6)?,
                        uid_validity: sql_u32(row.get::<_, i64>(7)?)?,
                        uid: sql_u32(row.get::<_, i64>(8)?)?,
                        date: sql_date(row.get(9)?),
                    },
                    senders: Vec::new(),
                })
            })?;
        let mut items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TEMP TABLE selected_list_ids (
                 list_id TEXT NOT NULL PRIMARY KEY
             ) WITHOUT ROWID;",
        )?;
        {
            let mut insert =
                transaction.prepare("INSERT INTO selected_list_ids (list_id) VALUES (?1)")?;
            for item in &items {
                insert.execute(params![item.list_id])?;
            }
        }
        transaction.commit()?;

        let mut previews: HashMap<String, Vec<String>> = HashMap::with_capacity(items.len());
        let mut preview_statement = connection.prepare(
            "WITH distinct_senders AS (
                 SELECT row.list_id, row.sender_email
                   FROM rank_rows AS row
                   JOIN selected_list_ids AS selected
                     ON selected.list_id = row.list_id
                  WHERE row.sender_email != ''
                  GROUP BY row.list_id, row.sender_email
             ),
             ranked_senders AS (
                 SELECT list_id, sender_email,
                        ROW_NUMBER() OVER (
                            PARTITION BY list_id ORDER BY sender_email
                        ) AS sender_rank
                   FROM distinct_senders
             )
             SELECT list_id, sender_email
               FROM ranked_senders
              WHERE sender_rank <= ?1
              ORDER BY list_id, sender_email",
        )?;
        let preview_rows = preview_statement
            .query_map(params![MAILING_LIST_SENDER_PREVIEW_LIMIT as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
        for row in preview_rows {
            let (list_id, sender) = row?;
            previews.entry(list_id).or_default().push(sender);
        }
        for item in &mut items {
            item.senders = previews.remove(&item.list_id).unwrap_or_default();
        }

        Ok(CachedRankPage {
            total_messages,
            total_groups,
            items,
        })
    })
    .await?
}

fn rank_totals(connection: &Connection, view: &str) -> CacheResult<(u64, u64)> {
    // `view` is always one of the fixed identifiers above, never caller
    // input. Keeping totals separate preserves them when `offset` is past EOF.
    let sql = format!("SELECT COUNT(*), COALESCE(SUM(message_count), 0) FROM {view}");
    connection
        .query_row(&sql, [], |row| {
            Ok((
                sql_u64(row.get::<_, i64>(0)?)?,
                sql_u64(row.get::<_, i64>(1)?)?,
            ))
        })
        .map_err(CacheError::from)
}

fn sql_rank_limit(limit: usize) -> CacheResult<i64> {
    i64::try_from(limit)
        .map_err(|_| CacheError::Invariant("rank page limit exceeds SQLite range".to_string()))
}

fn sql_offset(offset: usize) -> i64 {
    i64::try_from(offset).unwrap_or(i64::MAX)
}

fn sql_date(value: Option<i64>) -> Option<DateTime<Utc>> {
    value.and_then(DateTime::<Utc>::from_timestamp_millis)
}

async fn load_sync_state(path: Arc<PathBuf>, key: CacheKey) -> CacheResult<LoadedState> {
    tokio::task::spawn_blocking(move || {
        let connection = open_connection(&path)?;
        connection
            .query_row(
                "SELECT COALESCE(a.mutation_revision, 0),
                        m.uid_validity, m.uid_next, m.message_count, m.revision,
                        m.projection_version, m.account_revision,
                        COALESCE(q.header_fields_filtered, 0)
                   FROM (SELECT 1) AS singleton
                   LEFT JOIN account_state AS a ON a.account_key = ?1
                   LEFT JOIN mailbox_state AS m
                     ON m.account_key = ?1 AND m.mailbox = ?2
                   LEFT JOIN account_quirks AS q ON q.account_key = ?1",
                params![key.account, key.mailbox],
                |row| {
                    let account_revision = row.get(0)?;
                    let header_fields_filtered = row.get::<_, i64>(7)? != 0;
                    let uid_validity = row.get::<_, Option<i64>>(1)?.map(sql_u32).transpose()?;
                    let existing_revision = row.get::<_, Option<i64>>(4)?;
                    let projection_version = row.get::<_, Option<i64>>(5)?;
                    let mailbox = match (uid_validity, projection_version) {
                        (Some(uid_validity), Some(HEADER_PROJECTION_VERSION)) => {
                            Some(StoredState {
                                uid_validity,
                                uid_next: row.get::<_, Option<i64>>(2)?.map(sql_u32).transpose()?,
                                exists: sql_u32(row.get::<_, i64>(3)?)?,
                                account_revision: row.get::<_, Option<i64>>(6)?.unwrap_or_default(),
                            })
                        }
                        _ => None,
                    };
                    Ok(LoadedState {
                        mailbox,
                        existing_revision,
                        account_revision,
                        header_fields_filtered,
                    })
                },
            )
            .map_err(CacheError::from)
    })
    .await?
}

async fn load_state(path: Arc<PathBuf>, key: CacheKey) -> CacheResult<Option<StoredState>> {
    Ok(load_sync_state(path, key).await?.mailbox)
}

async fn load_membership(path: Arc<PathBuf>, key: CacheKey) -> CacheResult<Vec<u32>> {
    tokio::task::spawn_blocking(move || {
        let connection = open_connection(&path)?;
        let mut statement = connection.prepare(
            "SELECT uid FROM membership
              WHERE account_key = ?1 AND mailbox = ?2
              ORDER BY uid",
        )?;
        let rows = statement.query_map(params![key.account, key.mailbox], |row| {
            sql_u32(row.get::<_, i64>(0)?)
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(CacheError::from)
    })
    .await?
}

async fn load_header_uids(
    path: Arc<PathBuf>,
    key: CacheKey,
    uid_validity: u32,
) -> CacheResult<HashSet<u32>> {
    tokio::task::spawn_blocking(move || {
        let connection = open_connection(&path)?;
        let mut statement = connection.prepare(
            "SELECT uid FROM header_rows
              WHERE account_key = ?1 AND mailbox = ?2
                AND uid_validity = ?3 AND projection_version = ?4",
        )?;
        let rows = statement.query_map(
            params![
                key.account,
                key.mailbox,
                i64::from(uid_validity),
                HEADER_PROJECTION_VERSION
            ],
            |row| sql_u32(row.get::<_, i64>(0)?),
        )?;
        rows.collect::<std::result::Result<HashSet<_>, _>>()
            .map_err(CacheError::from)
    })
    .await?
}

async fn load_covered_count(
    path: Arc<PathBuf>,
    key: CacheKey,
    state: StoredState,
) -> CacheResult<u64> {
    tokio::task::spawn_blocking(move || {
        let connection = open_connection(&path)?;
        let count = connection.query_row(
            "SELECT COUNT(*)
               FROM membership AS m
               JOIN header_rows AS h
                 ON h.account_key = m.account_key
                AND h.mailbox = m.mailbox
                AND h.uid = m.uid
              WHERE m.account_key = ?1 AND m.mailbox = ?2
                AND h.uid_validity = ?3 AND h.projection_version = ?4",
            params![
                key.account,
                key.mailbox,
                i64::from(state.uid_validity),
                HEADER_PROJECTION_VERSION
            ],
            |row| sql_u64(row.get::<_, i64>(0)?),
        )?;
        Ok(count)
    })
    .await?
}

/// Delete one UID's projection row and membership marker atomically enough
/// for the completeness yardstick: both statements run on one connection, and
/// a covered row is only ever removed together with its membership.
async fn prune_uid_row(
    path: Arc<PathBuf>,
    key: CacheKey,
    uid_validity: u32,
    uid: u32,
) -> CacheResult<()> {
    tokio::task::spawn_blocking(move || {
        let mut connection = open_connection(&path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_uid_validity = transaction
            .query_row(
                "SELECT uid_validity FROM mailbox_state
                  WHERE account_key = ?1 AND mailbox = ?2",
                params![key.account, key.mailbox],
                |row| sql_u32(row.get::<_, i64>(0)?),
            )
            .optional()?;
        if current_uid_validity != Some(uid_validity) {
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "DELETE FROM header_rows
              WHERE account_key = ?1 AND mailbox = ?2
                AND uid_validity = ?3 AND uid = ?4",
            params![
                key.account,
                key.mailbox,
                i64::from(uid_validity),
                i64::from(uid)
            ],
        )?;
        transaction.execute(
            "DELETE FROM membership WHERE account_key = ?1 AND mailbox = ?2 AND uid = ?3",
            params![key.account, key.mailbox, i64::from(uid)],
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

/// Size of the walked membership set for one mailbox. In UID Mode the mailbox
/// EXISTS is only the visible-window count, so this — not EXISTS — is the
/// yardstick for whether the projection covers the whole mailbox.
async fn load_membership_count(path: Arc<PathBuf>, key: CacheKey) -> CacheResult<u64> {
    tokio::task::spawn_blocking(move || {
        let connection = open_connection(&path)?;
        let count = connection.query_row(
            "SELECT COUNT(*) FROM membership WHERE account_key = ?1 AND mailbox = ?2",
            params![key.account, key.mailbox],
            |row| sql_u64(row.get::<_, i64>(0)?),
        )?;
        Ok(count)
    })
    .await?
}

async fn store_headers(
    path: Arc<PathBuf>,
    key: CacheKey,
    uid_validity: u32,
    rows: Vec<ListHeaderRow>,
) -> CacheResult<()> {
    tokio::task::spawn_blocking(move || {
        let mut connection = open_connection(&path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO header_rows (
                     account_key, mailbox, uid_validity, uid, projection_version,
                     sender_email, sender_domain, sender_name, date_unix_ms,
                     message_id, list_id, list_display_name, has_list_headers,
                     advertised_one_click
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14
                 )
                 ON CONFLICT DO UPDATE SET
                     sender_email = excluded.sender_email,
                     sender_domain = excluded.sender_domain,
                     sender_name = excluded.sender_name,
                     date_unix_ms = excluded.date_unix_ms,
                     message_id = excluded.message_id,
                     list_id = excluded.list_id,
                     list_display_name = excluded.list_display_name,
                     has_list_headers = excluded.has_list_headers,
                     advertised_one_click = excluded.advertised_one_click",
            )?;
            for row in rows {
                let sender_email = crate::config::canonicalize_email(&row.sender_email)
                    .unwrap_or(row.sender_email);
                let sender_domain = domain_from_address(&sender_email).unwrap_or_default();
                let (list_id, list_display_name) = row
                    .list_id
                    .as_deref()
                    .and_then(normalized_list_id_fields)
                    .map_or((None, None), |(identifier, display)| {
                        (Some(identifier), Some(display))
                    });
                let has_list_headers =
                    row.list_unsubscribe.is_some() || row.list_unsubscribe_post.is_some();
                let advertised_one_click = crate::unsubscribe::advertises_one_click(
                    row.list_unsubscribe.as_deref(),
                    row.list_unsubscribe_post.as_deref(),
                );
                statement.execute(params![
                    key.account,
                    key.mailbox,
                    i64::from(uid_validity),
                    i64::from(row.uid),
                    HEADER_PROJECTION_VERSION,
                    sender_email,
                    sender_domain,
                    row.sender_name,
                    row.date.map(|date| date.timestamp_millis()),
                    row.message_id,
                    list_id,
                    list_display_name,
                    i64::from(has_list_headers),
                    i64::from(advertised_one_click),
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    })
    .await?
}

async fn publish_snapshot(
    path: Arc<PathBuf>,
    key: CacheKey,
    expected_revision: Option<i64>,
    expected_account_revision: i64,
    uid_validity: u32,
    uid_next: Option<u32>,
    live_uids: &[u32],
    cancel: Option<CancelFn>,
) -> CacheResult<PublishOutcome> {
    let live_uids = live_uids.to_vec();
    tokio::task::spawn_blocking(move || {
        let mut connection = open_connection(&path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let account_revision: i64 = transaction.query_row(
            "SELECT COALESCE(
                 (SELECT mutation_revision FROM account_state WHERE account_key = ?1),
                 0
             )",
            params![key.account],
            |row| row.get(0),
        )?;
        let current_revision = transaction
            .query_row(
                "SELECT revision FROM mailbox_state
                  WHERE account_key = ?1 AND mailbox = ?2",
                params![key.account, key.mailbox],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if current_revision != expected_revision || account_revision != expected_account_revision {
            return Ok(PublishOutcome::Conflict);
        }
        if cancel.as_ref().is_some_and(|check| check()) {
            return Err(CacheError::Cancelled);
        }

        transaction.execute(
            "DELETE FROM membership WHERE account_key = ?1 AND mailbox = ?2",
            params![key.account, key.mailbox],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO membership (account_key, mailbox, uid)
                 VALUES (?1, ?2, ?3)",
            )?;
            for (index, uid) in live_uids.iter().enumerate() {
                if index % 1_024 == 0 && cancel.as_ref().is_some_and(|check| check()) {
                    return Err(CacheError::Cancelled);
                }
                insert.execute(params![key.account, key.mailbox, i64::from(*uid)])?;
            }
        }
        if cancel.as_ref().is_some_and(|check| check()) {
            return Err(CacheError::Cancelled);
        }

        let covered: i64 = transaction.query_row(
            "SELECT COUNT(*)
               FROM membership AS m
               JOIN header_rows AS h
                 ON h.account_key = m.account_key
                AND h.mailbox = m.mailbox
                AND h.uid = m.uid
              WHERE m.account_key = ?1 AND m.mailbox = ?2
                AND h.uid_validity = ?3 AND h.projection_version = ?4",
            params![
                key.account,
                key.mailbox,
                i64::from(uid_validity),
                HEADER_PROJECTION_VERSION
            ],
            |row| row.get(0),
        )?;
        if covered != live_uids.len() as i64 {
            return Err(CacheError::Invariant(format!(
                "{} membership UIDs have only {covered} header markers",
                live_uids.len()
            )));
        }
        if cancel.as_ref().is_some_and(|check| check()) {
            return Err(CacheError::Cancelled);
        }

        let next_revision = expected_revision.map_or(1, |value| value + 1);
        transaction.execute(
            "INSERT INTO mailbox_state (
                 account_key, mailbox, uid_validity, uid_next, message_count,
                 revision, projection_version, account_revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (account_key, mailbox) DO UPDATE SET
                 uid_validity = excluded.uid_validity,
                 uid_next = excluded.uid_next,
                 message_count = excluded.message_count,
                 revision = excluded.revision,
                 projection_version = excluded.projection_version,
                 account_revision = excluded.account_revision",
            params![
                key.account,
                key.mailbox,
                i64::from(uid_validity),
                uid_next.map(i64::from),
                live_uids.len() as i64,
                next_revision,
                HEADER_PROJECTION_VERSION,
                account_revision,
            ],
        )?;
        transaction.execute(
            "DELETE FROM header_rows
              WHERE account_key = ?1 AND mailbox = ?2
                AND (
                    uid_validity != ?3 OR projection_version != ?4 OR
                    NOT EXISTS (
                        SELECT 1 FROM membership AS m
                         WHERE m.account_key = header_rows.account_key
                           AND m.mailbox = header_rows.mailbox
                           AND m.uid = header_rows.uid
                    )
                )",
            params![
                key.account,
                key.mailbox,
                i64::from(uid_validity),
                HEADER_PROJECTION_VERSION
            ],
        )?;
        if cancel.as_ref().is_some_and(|check| check()) {
            return Err(CacheError::Cancelled);
        }
        transaction.commit()?;
        Ok(PublishOutcome::Published)
    })
    .await?
}

async fn advance_account_revision(path: Arc<PathBuf>, account: String) -> CacheResult<()> {
    tokio::task::spawn_blocking(move || {
        let mut connection = open_connection(&path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO account_state (account_key, mutation_revision)
             VALUES (?1, 1)
             ON CONFLICT (account_key) DO UPDATE SET
                 mutation_revision = account_state.mutation_revision + 1",
            params![account],
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

fn sql_u32(value: i64) -> rusqlite::Result<u32> {
    u32::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn sql_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn test_key(mailbox: &str) -> CacheKey {
        CacheKey {
            account: "test-account".to_string(),
            mailbox: mailbox.to_string(),
        }
    }

    fn test_row(uid: u32) -> ListHeaderRow {
        ListHeaderRow {
            uid,
            uid_validity: Some(10),
            list_unsubscribe: None,
            list_unsubscribe_post: None,
            list_id: None,
            sender_email: format!("sender{uid}@example.com"),
            sender_name: String::new(),
            date: None,
            message_id: Some(format!("<{uid}@example.com>")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rank_row(
        uid: u32,
        sender_email: &str,
        sender_name: &str,
        date_unix_ms: Option<i64>,
        message_id: Option<&str>,
        list_id: Option<&str>,
        has_list_headers: bool,
        advertised_one_click: bool,
    ) -> ListHeaderRow {
        ListHeaderRow {
            uid,
            uid_validity: Some(7),
            list_unsubscribe: if advertised_one_click {
                Some(format!(
                    "<https://unsubscribe.example.test/{uid}?recipient=secret-{uid}>"
                ))
            } else if has_list_headers {
                Some(format!("<mailto:list-{uid}@example.test>"))
            } else {
                None
            },
            list_unsubscribe_post: advertised_one_click
                .then(|| "List-Unsubscribe=One-Click".to_string()),
            list_id: list_id.map(str::to_string),
            sender_email: sender_email.to_string(),
            sender_name: sender_name.to_string(),
            date: date_unix_ms.and_then(DateTime::<Utc>::from_timestamp_millis),
            message_id: message_id.map(str::to_string),
        }
    }

    async fn publish_test_rows(cache: &HeaderCache, mailbox: &str, rows: Vec<ListHeaderRow>) {
        let key = test_key(mailbox);
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let uids: Vec<u32> = rows.iter().map(|row| row.uid).collect();
        store_headers(path.clone(), key.clone(), 7, rows)
            .await
            .expect("store ranking rows");
        let outcome = publish_snapshot(
            path,
            key,
            None,
            0,
            7,
            uids.iter().max().and_then(|uid| uid.checked_add(1)),
            &uids,
            None,
        )
        .await
        .expect("publish ranking rows");
        assert_eq!(outcome, PublishOutcome::Published);
    }

    fn test_scope(cache: &HeaderCache, mailboxes: &[&str]) -> RankScope {
        RankScope {
            path: Arc::clone(cache.path.as_ref().expect("test cache path")),
            account: "test-account".to_string(),
            mailboxes: mailboxes
                .iter()
                .map(|mailbox| mailbox.to_string())
                .collect(),
        }
    }

    fn reference_sender_page(
        mut rows: Vec<(String, ListHeaderRow)>,
        own_addresses: &HashSet<String>,
        offset: usize,
        limit: usize,
    ) -> CachedRankPage<CachedSenderRank> {
        rows.sort_by(|(left_mailbox, left), (right_mailbox, right)| {
            (right.date, right_mailbox, right.uid).cmp(&(left.date, left_mailbox, left.uid))
        });
        let mut seen = HashSet::new();
        let mut groups: HashMap<(String, String), CachedSenderRank> = HashMap::new();
        for (mailbox, row) in rows {
            if row.sender_email.is_empty() || own_addresses.contains(&row.sender_email) {
                continue;
            }
            if let Some(message_id) = row.message_id.as_deref().map(str::trim)
                && !message_id.is_empty()
                && !seen.insert(message_id.to_string())
            {
                continue;
            }
            let key = (row.sender_email.clone(), row.sender_name.clone());
            let candidate = CachedRankSample {
                mailbox,
                uid_validity: row.uid_validity.expect("test UIDVALIDITY"),
                uid: row.uid,
                date: row.date,
            };
            let group = groups.entry(key).or_insert_with(|| CachedSenderRank {
                address: row.sender_email,
                display_name: row.sender_name,
                count: 0,
                oldest_date: None,
                newest_date: None,
                sample: candidate.clone(),
            });
            group.count += 1;
            if let Some(date) = row.date {
                group.oldest_date = Some(group.oldest_date.map_or(date, |oldest| oldest.min(date)));
                group.newest_date = Some(group.newest_date.map_or(date, |newest| newest.max(date)));
            }
            if (candidate.date, &candidate.mailbox, candidate.uid)
                > (group.sample.date, &group.sample.mailbox, group.sample.uid)
            {
                group.sample = candidate;
            }
        }
        let mut items: Vec<_> = groups.into_values().collect();
        items.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.address.cmp(&right.address))
                .then_with(|| left.display_name.cmp(&right.display_name))
        });
        let total_groups = items.len() as u64;
        let total_messages = items.iter().map(|item| item.count).sum();
        let items = items.into_iter().skip(offset).take(limit).collect();
        CachedRankPage {
            total_messages,
            total_groups,
            items,
        }
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("agentmail-cache-test-{}", uuid::Uuid::new_v4()))
            .join(format!("{name}.sqlite3"))
    }

    /// Pruning a stale ranking sample removes the projection row AND its
    /// membership marker together, so the covered==membership completeness
    /// yardstick still hits and the survivor is the only remaining sample.
    #[tokio::test]
    async fn prune_uid_removes_row_and_membership_together() {
        let cache = HeaderCache::at_path(test_path("prune-stale-sample"));
        publish_test_rows(
            &cache,
            "INBOX",
            vec![
                rank_row(
                    1,
                    "gone@example.com",
                    "Gone",
                    Some(1_000),
                    Some("<m1@example.com>"),
                    Some("List <l.example.com>"),
                    true,
                    false,
                ),
                rank_row(
                    2,
                    "alive@example.com",
                    "Alive",
                    Some(2_000),
                    Some("<m2@example.com>"),
                    None,
                    false,
                    false,
                ),
            ],
        )
        .await;

        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        prune_uid_row(Arc::clone(&path), test_key("INBOX"), 7, 1)
            .await
            .expect("prune succeeds");
        prune_uid_row(Arc::clone(&path), test_key("INBOX"), 6, 2)
            .await
            .expect("stale-epoch prune is a safe no-op");

        let (rows, members, survivor) = tokio::task::spawn_blocking(move || {
            let connection = open_connection(&path)?;
            let rows: i64 =
                connection.query_row("SELECT COUNT(*) FROM header_rows", [], |row| row.get(0))?;
            let members: i64 =
                connection.query_row("SELECT COUNT(*) FROM membership", [], |row| row.get(0))?;
            let survivor: i64 =
                connection.query_row("SELECT uid FROM header_rows", [], |row| row.get(0))?;
            Ok::<_, CacheError>((rows, members, survivor))
        })
        .await
        .expect("join")
        .expect("query");
        assert_eq!(rows, 1, "only the stale row is removed");
        assert_eq!(
            members, 1,
            "membership pruned with the row — completeness stays covered==members"
        );
        assert_eq!(survivor, 2);
    }

    #[test]
    fn header_cache_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HeaderCache>();
    }

    #[test]
    fn unchanged_complete_state_is_a_hit() {
        let state = StoredState {
            uid_validity: 10,
            uid_next: Some(20),
            exists: 3,
            account_revision: 0,
        };
        let status = MailboxStatus {
            uid_validity: Some(10),
            uid_next: Some(20),
            exists: 3,
            highest_modseq: Some(999),
        };
        assert!(is_cache_hit(state, &status, 0, false));
    }

    #[test]
    fn local_mutation_fence_prevents_a_hit_when_the_triple_is_unchanged() {
        // Yahoo/AOL windowed INBOX: after a local delete, EXISTS backfills to
        // the same value and UIDNEXT is unmoved — only the fence detects it.
        let state = StoredState {
            uid_validity: 10,
            uid_next: Some(20),
            exists: 3,
            account_revision: 4,
        };
        let status = MailboxStatus {
            uid_validity: Some(10),
            uid_next: Some(20),
            exists: 3,
            highest_modseq: None,
        };
        assert!(is_cache_hit(state, &status, 4, false));
        assert!(
            !is_cache_hit(state, &status, 5, false),
            "a fence bump after publish must force a membership resync"
        );
    }

    #[test]
    fn uid_mode_hit_ignores_the_windowed_exists() {
        // UID Mode stores the full mailbox count; EXAMINE still reports the
        // window, so a hit must not require EXISTS to match — only UIDVALIDITY,
        // UIDNEXT, and the fence.
        let state = StoredState {
            uid_validity: 10,
            uid_next: Some(500),
            exists: 435_000, // full mailbox, from the PARTIAL walk
            account_revision: 0,
        };
        let status = MailboxStatus {
            uid_validity: Some(10),
            uid_next: Some(500),
            exists: 100_000, // windowed EXAMINE
            highest_modseq: None,
        };
        assert!(
            is_cache_hit(state, &status, 0, true),
            "UID Mode hit rests on UIDNEXT + fence, not the windowed EXISTS"
        );
        assert!(
            !is_cache_hit(state, &status, 0, false),
            "Limited Mode still requires EXISTS to match"
        );
    }

    #[test]
    fn uid_mode_completeness_measures_membership_not_the_window() {
        // The healed projection covers the whole mailbox (434_894 rows) while
        // EXAMINE still reports the 1000-message window. Measuring completeness
        // against EXISTS would never match and re-walk every warm call; against
        // membership it serves the hit.
        assert!(
            projection_is_complete(true, 434_894, 434_894),
            "full coverage of the walked membership is a complete UID-Mode hit"
        );
        assert!(
            !projection_is_complete(true, 1_000, 434_894),
            "the truncated single-window projection must reconcile, not hit"
        );
        assert!(
            !projection_is_complete(true, 0, 0),
            "an unwalked UID-Mode mailbox must sync, not serve an empty hit"
        );
    }

    #[test]
    fn limited_mode_completeness_still_measures_exists() {
        assert!(
            projection_is_complete(false, 3, 3),
            "Limited Mode is complete when every EXISTS message has a row"
        );
        assert!(
            !projection_is_complete(false, 2, 3),
            "a missing row in Limited Mode reconciles"
        );
        assert!(
            projection_is_complete(false, 0, 0),
            "a genuinely empty Limited-Mode mailbox is a valid hit"
        );
    }

    #[test]
    fn missing_uidnext_prevents_a_hit() {
        let state = StoredState {
            uid_validity: 10,
            uid_next: None,
            exists: 3,
            account_revision: 0,
        };
        let status = MailboxStatus {
            uid_validity: Some(10),
            uid_next: Some(20),
            exists: 3,
            highest_modseq: None,
        };
        assert!(!is_cache_hit(state, &status, 0, false));
    }

    #[test]
    fn tail_count_proves_append_even_with_uid_gaps() {
        let state = StoredState {
            uid_validity: 10,
            uid_next: Some(20),
            exists: 3,
            account_revision: 0,
        };
        let status = MailboxStatus {
            uid_validity: Some(10),
            uid_next: Some(30),
            exists: 5,
            highest_modseq: None,
        };
        assert!(pure_append_is_proven(state, &status, &[1, 2, 3], &[20, 29]));
    }

    #[test]
    fn tail_count_exposes_an_old_message_deletion() {
        let state = StoredState {
            uid_validity: 10,
            uid_next: Some(20),
            exists: 3,
            account_revision: 0,
        };
        let status = MailboxStatus {
            uid_validity: Some(10),
            uid_next: Some(22),
            exists: 3,
            highest_modseq: None,
        };
        assert!(!pure_append_is_proven(state, &status, &[1, 2, 3], &[20]));
    }

    #[test]
    fn tail_proof_rejects_corrupt_or_overlapping_membership() {
        let state = StoredState {
            uid_validity: 10,
            uid_next: Some(20),
            exists: 3,
            account_revision: 0,
        };
        let status = MailboxStatus {
            uid_validity: Some(10),
            uid_next: Some(22),
            exists: 4,
            highest_modseq: None,
        };
        assert!(!pure_append_is_proven(state, &status, &[1, 2, 20], &[20]));
        assert!(!pure_append_is_proven(
            state,
            &status,
            &[1, 2, 3],
            &[20, 20]
        ));
    }

    #[tokio::test]
    async fn published_snapshot_survives_a_new_cache_instance() {
        let path = test_path("restart");
        let cache = HeaderCache::at_path(path.clone());
        let key = test_key("INBOX");
        store_headers(
            Arc::clone(cache.path.as_ref().expect("test cache path")),
            key.clone(),
            7,
            vec![test_row(1), test_row(2)],
        )
        .await
        .expect("store headers");
        let outcome = publish_snapshot(
            Arc::clone(cache.path.as_ref().expect("test cache path")),
            key.clone(),
            None,
            0,
            7,
            Some(3),
            &[1, 2],
            None,
        )
        .await
        .expect("publish snapshot");
        assert_eq!(outcome, PublishOutcome::Published);

        let reopened = HeaderCache::at_path(path);
        let state = load_state(
            Arc::clone(reopened.path.as_ref().expect("test cache path")),
            key.clone(),
        )
        .await
        .expect("load state")
        .expect("published state");
        let covered = load_covered_count(
            Arc::clone(reopened.path.as_ref().expect("test cache path")),
            key,
            state,
        )
        .await
        .expect("count rows");
        assert_eq!(covered, 2);
    }

    #[test]
    fn fetch_chunk_reconcile_prunes_gaps_but_rejects_a_whole_missing_chunk() {
        let row = |uid| ListHeaderRow {
            uid,
            uid_validity: Some(10),
            list_unsubscribe: None,
            list_unsubscribe_post: None,
            list_id: None,
            sender_email: String::new(),
            sender_name: String::new(),
            date: None,
            message_id: None,
        };

        // Partial omission (UID 2 expunged between SEARCH and FETCH): prune it.
        let mut live = vec![1, 2, 3];
        reconcile_fetch_chunk("INBOX", &[1, 2, 3], &[row(1), row(3)], &mut live)
            .expect("partial omission reconciles");
        assert_eq!(live, vec![1, 3]);

        // Whole non-empty chunk empty (swallowed server rejection): error,
        // and membership is left untouched for the resume path.
        let mut live = vec![1, 2, 3];
        let error = reconcile_fetch_chunk("INBOX", &[1, 2, 3], &[], &mut live)
            .expect_err("a wholly-empty chunk must not silently prune");
        assert!(error.to_string().contains("swallowed server rejection"));
        assert_eq!(live, vec![1, 2, 3]);

        // An empty request is a no-op, never an error.
        let mut live = vec![1];
        reconcile_fetch_chunk("INBOX", &[], &[], &mut live).expect("empty request is fine");
        assert_eq!(live, vec![1]);
    }

    #[test]
    fn cache_key_ignores_the_account_display_name() {
        let config = AccountConfig {
            host: "export.imap.aol.com".to_string(),
            port: 993,
            username: "user@verizon.net".to_string(),
            email: None,
            aliases: Vec::new(),
            password: None,
            tls: true,
            max_connections: None,
            auth: crate::config::AuthMethod::Password,
        };
        // Renaming the account points at the same mailbox → same projection.
        let renamed = CacheKey::new("Cthrower", &config, "INBOX");
        let original = CacheKey::new("Custom", &config, "INBOX");
        assert_eq!(renamed.account, original.account);
        // A different login is still a different mailbox.
        let other = AccountConfig {
            username: "someone-else@verizon.net".to_string(),
            ..config.clone()
        };
        assert_ne!(
            CacheKey::new("Custom", &config, "INBOX").account,
            CacheKey::new("Custom", &other, "INBOX").account
        );
    }

    #[tokio::test]
    async fn poisoned_projection_is_detected_from_stored_state() {
        let cache = HeaderCache::at_path(test_path("poison-detect"));
        let key = test_key("INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));

        // FIELDS-mode build: many rows carry a List-Id, none an unsubscribe
        // header — the AOL/Yahoo poisoning signature.
        let poisoned: Vec<ListHeaderRow> = (0..imap_client::QUIRK_MIN_LIST_ID_ROWS as u32)
            .map(|uid| {
                let mut row = test_row(uid + 1);
                row.list_id = Some("<bulk.example.com>".to_string());
                row // list_unsubscribe stays None
            })
            .collect();
        store_headers(path.clone(), key.clone(), 10, poisoned)
            .await
            .expect("store poisoned rows");
        assert!(
            cache.mailbox_projection_poisoned(&path, &key, 10).await,
            "list-id rows with zero unsubscribe flags read as poisoned"
        );

        // Healing one row (an unsubscribe header now present) clears it — the
        // atomic heal guarantees this only happens on a complete refetch.
        let mut healed = test_row(1);
        healed.list_id = Some("<bulk.example.com>".to_string());
        healed.list_unsubscribe = Some("<https://example.com/u>".to_string());
        store_headers(path.clone(), key.clone(), 10, vec![healed])
            .await
            .expect("store one healed row");
        assert!(
            !cache.mailbox_projection_poisoned(&path, &key, 10).await,
            "any surviving unsubscribe flag clears the poison signal"
        );

        // A different UIDVALIDITY epoch has no rows → not poisoned.
        assert!(!cache.mailbox_projection_poisoned(&path, &key, 11).await);
    }

    #[tokio::test]
    async fn header_fields_quirk_survives_a_process_restart() {
        let path_buf = test_path("quirk-persist");
        let cache = HeaderCache::at_path(path_buf.clone());
        let key = test_key("INBOX");

        cache.persist_account_quirky(&key.account).await;

        // A fresh instance at the same path models an app restart: the
        // in-process set is empty, but the persisted flag must load.
        let restarted = HeaderCache::at_path(path_buf);
        assert!(
            !restarted.account_is_quirky(&key.account),
            "in-process memory does not survive the restart"
        );
        let path = Arc::clone(restarted.path.as_ref().expect("test cache path"));
        let loaded = load_sync_state(path, key)
            .await
            .expect("load state after restart");
        assert!(
            loaded.header_fields_filtered,
            "the persisted quirk flag must survive and force full-header syncs"
        );
    }

    #[tokio::test]
    async fn cached_list_id_count_reports_projected_matches_per_mailbox() {
        let cache = HeaderCache::at_path(test_path("list-id-count"));
        let config = AccountConfig {
            host: "imap.example.com".to_string(),
            port: 993,
            username: "user@example.com".to_string(),
            email: None,
            aliases: Vec::new(),
            password: None,
            tls: true,
            max_connections: None,
            auth: crate::config::AuthMethod::Password,
        };
        let key = CacheKey::new("acct", &config, "INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let mut listed = test_row(1);
        listed.list_id = Some("<mylist.example.com>".to_string());
        let mut other = test_row(2);
        other.list_id = Some("<other.example.com>".to_string());
        store_headers(path, key, 10, vec![listed, other, test_row(3)])
            .await
            .expect("store headers");

        assert_eq!(
            cache
                .cached_list_id_uids("acct", &config, "INBOX", "mylist.example.com", 10)
                .await,
            vec![1],
            "the projection knows which INBOX message carries the list"
        );
        assert!(
            cache
                .cached_list_id_uids("acct", &config, "Archive", "mylist.example.com", 10)
                .await
                .is_empty(),
            "candidates are per mailbox"
        );
        assert!(
            cache
                .cached_list_id_uids("acct", &config, "INBOX", "mylist.example.com", 11)
                .await
                .is_empty(),
            "a different UIDVALIDITY epoch yields no candidates"
        );
        assert!(
            cache
                .cached_list_id_uids("acct", &config, "INBOX", "unknown.example.com", 10)
                .await
                .is_empty(),
            "unknown lists yield no candidates"
        );
    }

    #[tokio::test]
    async fn cached_domain_uids_are_canonical_exact_and_published_only() {
        let cache = HeaderCache::at_path(test_path("domain-uids"));
        let config = AccountConfig {
            host: "imap.example.com".to_string(),
            port: 993,
            username: "user@example.com".to_string(),
            email: None,
            aliases: Vec::new(),
            password: None,
            tls: true,
            max_connections: None,
            auth: crate::config::AuthMethod::Password,
        };
        let key = CacheKey::new("acct", &config, "INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let mut child = test_row(1);
        child.sender_email = "one@MAIL.Example.COM".to_string();
        let mut parent = test_row(2);
        parent.sender_email = "two@example.com".to_string();
        let mut invalid = test_row(3);
        invalid.sender_email = "literal@127.0.0.1".to_string();
        store_headers(path.clone(), key.clone(), 10, vec![child, parent, invalid])
            .await
            .expect("store domain rows");
        publish_snapshot(
            path.clone(),
            key.clone(),
            None,
            0,
            10,
            Some(4),
            &[1, 2, 3],
            None,
        )
        .await
        .expect("publish domain rows");

        assert_eq!(
            cache
                .cached_domain_uids("acct", &config, "inbox", "mail.example.com.", 10)
                .await,
            vec![1]
        );
        assert_eq!(
            cache
                .cached_domain_uids("acct", &config, "INBOX", "example.com", 10)
                .await,
            vec![2],
            "a parent domain must not include its subdomains"
        );
        assert!(
            cache
                .cached_domain_uids("acct", &config, "INBOX", "mail.example.com", 11)
                .await
                .is_empty()
        );
        assert!(
            cache
                .cached_domain_uids("acct", &config, "INBOX", "[127.0.0.1]", 10)
                .await
                .is_empty()
        );

        let mut next_epoch = test_row(1);
        next_epoch.sender_email = "overlap@mail.example.com".to_string();
        store_headers(path.clone(), key.clone(), 11, vec![next_epoch])
            .await
            .expect("store unpublished next-epoch row");
        assert!(
            cache
                .cached_domain_uids("acct", &config, "INBOX", "mail.example.com", 11)
                .await
                .is_empty(),
            "numeric UID overlap cannot expose an unpublished mailbox epoch"
        );

        let mut unpublished = test_row(4);
        unpublished.sender_email = "late@mail.example.com".to_string();
        store_headers(path, key, 10, vec![unpublished])
            .await
            .expect("store unpublished row");
        assert_eq!(
            cache
                .cached_domain_uids("acct", &config, "INBOX", "mail.example.com", 10)
                .await,
            vec![1],
            "unpublished fetch rows are not mutation candidates"
        );
    }

    #[tokio::test]
    async fn account_revision_fences_an_in_flight_publisher() {
        let path = test_path("mutation-fence");
        let cache = HeaderCache::at_path(path);
        let key = test_key("INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        store_headers(path.clone(), key.clone(), 7, vec![test_row(1)])
            .await
            .expect("store headers");
        publish_snapshot(path.clone(), key.clone(), None, 0, 7, Some(2), &[1], None)
            .await
            .expect("publish initial snapshot");
        let before = load_sync_state(path.clone(), key.clone())
            .await
            .expect("load state");
        assert!(before.mailbox.is_some());
        advance_account_revision(path.clone(), key.account.clone())
            .await
            .expect("advance mutation revision");
        let outcome = publish_snapshot(
            path,
            key,
            before.existing_revision,
            before.account_revision,
            7,
            Some(2),
            &[1],
            None,
        )
        .await
        .expect("publish conflict result");
        assert_eq!(outcome, PublishOutcome::Conflict);
    }

    #[tokio::test]
    async fn account_revision_fences_the_first_cold_publisher() {
        let cache = HeaderCache::at_path(test_path("cold-dirty-fence"));
        let key = test_key("INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let before = load_sync_state(path.clone(), key.clone())
            .await
            .expect("load empty state");
        assert!(before.mailbox.is_none());
        assert_eq!(before.existing_revision, None);

        store_headers(path.clone(), key.clone(), 7, vec![test_row(1)])
            .await
            .expect("store partial cold-scan row");
        advance_account_revision(path.clone(), key.account.clone())
            .await
            .expect("mark account dirty");

        let outcome = publish_snapshot(
            path.clone(),
            key.clone(),
            before.existing_revision,
            before.account_revision,
            7,
            Some(2),
            &[1],
            None,
        )
        .await
        .expect("publish conflict result");
        assert_eq!(outcome, PublishOutcome::Conflict);
        assert!(load_state(path, key).await.expect("load state").is_none());
    }

    #[tokio::test]
    async fn cancellation_rolls_back_membership_publication() {
        let cache = HeaderCache::at_path(test_path("cancel-publication"));
        let key = test_key("INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        store_headers(path.clone(), key.clone(), 7, vec![test_row(1), test_row(2)])
            .await
            .expect("store headers");
        publish_snapshot(path.clone(), key.clone(), None, 0, 7, Some(2), &[1], None)
            .await
            .expect("publish initial snapshot");
        let before = load_sync_state(path.clone(), key.clone())
            .await
            .expect("load state");
        let cancel: CancelFn = Arc::new(|| true);

        let result = publish_snapshot(
            path.clone(),
            key.clone(),
            before.existing_revision,
            before.account_revision,
            7,
            Some(3),
            &[1, 2],
            Some(cancel),
        )
        .await;
        assert!(matches!(result, Err(CacheError::Cancelled)));
        assert_eq!(
            load_membership(path, key).await.expect("load membership"),
            vec![1]
        );
    }

    #[tokio::test]
    async fn cancellation_rolls_back_after_membership_insertion() {
        let cache = HeaderCache::at_path(test_path("late-cancel-publication"));
        let key = test_key("INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        store_headers(path.clone(), key.clone(), 7, vec![test_row(1), test_row(2)])
            .await
            .expect("store headers");
        publish_snapshot(path.clone(), key.clone(), None, 0, 7, Some(2), &[1], None)
            .await
            .expect("publish initial snapshot");
        let before = load_sync_state(path.clone(), key.clone())
            .await
            .expect("load state");
        let checks = Arc::new(AtomicUsize::new(0));
        let cancel_checks = Arc::clone(&checks);
        let cancel: CancelFn = Arc::new(move || cancel_checks.fetch_add(1, Ordering::SeqCst) >= 2);

        let result = publish_snapshot(
            path.clone(),
            key.clone(),
            before.existing_revision,
            before.account_revision,
            7,
            Some(3),
            &[1, 2],
            Some(cancel),
        )
        .await;
        assert!(matches!(result, Err(CacheError::Cancelled)));
        assert!(checks.load(Ordering::SeqCst) >= 3);
        assert_eq!(
            load_membership(path, key).await.expect("load membership"),
            vec![1]
        );
    }

    #[tokio::test]
    async fn empty_publication_honors_cancellation() {
        let cache = HeaderCache::at_path(test_path("empty-cancel-publication"));
        let key = test_key("INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let cancel: CancelFn = Arc::new(|| true);

        let result = publish_snapshot(
            path.clone(),
            key.clone(),
            None,
            0,
            7,
            Some(1),
            &[],
            Some(cancel),
        )
        .await;
        assert!(matches!(result, Err(CacheError::Cancelled)));
        assert!(load_state(path, key).await.expect("load state").is_none());
    }

    #[tokio::test]
    async fn projection_upgrade_can_replace_an_existing_state_row() {
        let cache = HeaderCache::at_path(test_path("projection-upgrade"));
        let key = test_key("INBOX");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        store_headers(path.clone(), key.clone(), 7, vec![test_row(1)])
            .await
            .expect("store headers");
        publish_snapshot(path.clone(), key.clone(), None, 0, 7, Some(2), &[1], None)
            .await
            .expect("publish initial snapshot");

        let edit_path = path.clone();
        let edit_key = key.clone();
        tokio::task::spawn_blocking(move || {
            let connection = open_connection(&edit_path)?;
            connection.execute(
                "UPDATE mailbox_state SET projection_version = 0
                  WHERE account_key = ?1 AND mailbox = ?2",
                params![edit_key.account, edit_key.mailbox],
            )?;
            CacheResult::Ok(())
        })
        .await
        .expect("join projection edit")
        .expect("edit projection");

        let stale = load_sync_state(path.clone(), key.clone())
            .await
            .expect("load stale state");
        assert!(stale.mailbox.is_none());
        assert!(stale.existing_revision.is_some());
        let outcome = publish_snapshot(
            path,
            key,
            stale.existing_revision,
            stale.account_revision,
            7,
            Some(2),
            &[1],
            None,
        )
        .await
        .expect("replace stale projection");
        assert_eq!(outcome, PublishOutcome::Published);
    }

    #[tokio::test]
    async fn schema_contains_only_the_documented_header_projection() {
        let cache = HeaderCache::at_path(test_path("schema-privacy"));
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let columns = tokio::task::spawn_blocking(move || {
            let connection = open_connection(&path)?;
            let mut statement = connection.prepare("PRAGMA table_info(header_rows)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(CacheError::from)
        })
        .await
        .expect("join schema inspection")
        .expect("inspect schema");

        assert_eq!(
            columns,
            [
                "account_key",
                "mailbox",
                "uid_validity",
                "uid",
                "projection_version",
                "sender_email",
                "sender_domain",
                "sender_name",
                "date_unix_ms",
                "message_id",
                "list_id",
                "list_display_name",
                "has_list_headers",
                "advertised_one_click",
            ]
        );
    }

    #[test]
    fn cache_uses_wal_mode() {
        let path = test_path("wal-mode");
        let connection = open_connection(&path).expect("open cache");
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("read journal mode");
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[tokio::test]
    async fn projection_stores_list_identity_and_booleans_without_unsubscribe_tokens() {
        let cache = HeaderCache::at_path(test_path("token-free-projection"));
        publish_test_rows(
            &cache,
            "INBOX",
            vec![rank_row(
                1,
                "list@example.com",
                "List Sender",
                Some(1_000),
                Some("<message@example.com>"),
                Some("Friendly List <NEWS.Example.COM>"),
                true,
                true,
            )],
        )
        .await;

        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let stored = tokio::task::spawn_blocking(move || {
            let connection = open_connection(&path)?;
            connection
                .query_row(
                    "SELECT sender_domain, list_id, list_display_name,
                            has_list_headers, advertised_one_click
                       FROM header_rows",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    },
                )
                .map_err(CacheError::from)
        })
        .await
        .expect("join projection read")
        .expect("read projection");
        assert_eq!(
            stored,
            (
                "example.com".to_string(),
                Some("news.example.com".to_string()),
                Some("Friendly List".to_string()),
                1,
                1,
            )
        );

        let path = cache.path.as_ref().expect("test cache path");
        for candidate in [
            path.as_ref().clone(),
            PathBuf::from(format!("{}-wal", path.display())),
        ] {
            if candidate.exists() {
                let bytes = std::fs::read(candidate).expect("read cache file");
                let cache_text = String::from_utf8_lossy(&bytes);
                assert!(!cache_text.contains("recipient=secret-1"));
                assert!(!cache_text.contains("mailto:list-1@example.test"));
            }
        }
    }

    #[tokio::test]
    async fn sql_sender_ranking_deduplicates_message_ids_and_pages() {
        let cache = HeaderCache::at_path(test_path("sender-ranking"));
        let inbox_rows = vec![
            rank_row(
                1,
                "alpha@example.com",
                "Alpha",
                Some(1_000),
                Some(" <shared@example.com> "),
                None,
                false,
                false,
            ),
            rank_row(
                2,
                "alpha@example.com",
                "Alpha",
                Some(3_000),
                Some("<alpha-new@example.com>"),
                None,
                false,
                false,
            ),
            rank_row(
                3,
                "beta@example.com",
                "Beta",
                Some(2_000),
                None,
                None,
                false,
                false,
            ),
            rank_row(
                4,
                "owner@example.com",
                "Owner",
                Some(4_000),
                None,
                None,
                false,
                false,
            ),
        ];
        let archive_rows = vec![
            rank_row(
                99,
                "alpha@example.com",
                "Alpha",
                Some(1_000),
                Some("<shared@example.com>"),
                None,
                false,
                false,
            ),
            rank_row(
                100,
                "beta@example.com",
                "Beta",
                Some(2_500),
                Some("   "),
                None,
                false,
                false,
            ),
        ];
        publish_test_rows(&cache, "INBOX", inbox_rows.clone()).await;
        publish_test_rows(&cache, "Archive", archive_rows.clone()).await;

        let scope = test_scope(&cache, &["INBOX", "Archive"]);
        let page = query_sender_page(scope.clone(), vec!["owner@example.com".to_string()], 0, 1)
            .await
            .expect("query first sender page");
        assert_eq!(page.total_groups, 2);
        assert_eq!(page.total_messages, 4);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].address, "alpha@example.com");
        assert_eq!(page.items[0].count, 2);
        assert_eq!(page.items[0].sample.mailbox, "INBOX");
        assert_eq!(page.items[0].sample.uid, 2);

        let reference_rows = inbox_rows
            .into_iter()
            .map(|row| ("INBOX".to_string(), row))
            .chain(
                archive_rows
                    .into_iter()
                    .map(|row| ("Archive".to_string(), row)),
            )
            .collect();
        let own = HashSet::from(["owner@example.com".to_string()]);
        assert_eq!(page, reference_sender_page(reference_rows, &own, 0, 1));

        let second = query_sender_page(scope, vec!["owner@example.com".to_string()], 1, 10)
            .await
            .expect("query second sender page");
        assert_eq!(second.total_groups, 2);
        assert_eq!(second.total_messages, 4);
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].address, "beta@example.com");
        assert_eq!(second.items[0].count, 2);
    }

    #[tokio::test]
    async fn sql_domain_ranking_is_exact_canonical_deduplicated_and_paged() {
        let cache = HeaderCache::at_path(test_path("domain-ranking"));
        publish_test_rows(
            &cache,
            "INBOX",
            vec![
                rank_row(
                    1,
                    "alpha@Example.COM",
                    "Alpha",
                    Some(1_000),
                    Some("<shared@example.com>"),
                    None,
                    false,
                    false,
                ),
                rank_row(
                    2,
                    "beta@MAIL.Example.com",
                    "Beta",
                    Some(3_000),
                    Some("<beta@example.com>"),
                    None,
                    false,
                    false,
                ),
                rank_row(
                    3,
                    "gamma@mail.example.com",
                    "Gamma",
                    Some(2_000),
                    Some("<gamma@example.com>"),
                    None,
                    false,
                    false,
                ),
                rank_row(
                    4,
                    "owner@owner.example",
                    "Owner",
                    Some(4_000),
                    None,
                    None,
                    false,
                    false,
                ),
                rank_row(
                    5,
                    "malformed-address",
                    "Malformed",
                    Some(5_000),
                    None,
                    None,
                    false,
                    false,
                ),
                rank_row(
                    6,
                    "reader@BÜCHER.DE",
                    "Reader",
                    Some(6_000),
                    None,
                    None,
                    false,
                    false,
                ),
            ],
        )
        .await;
        publish_test_rows(
            &cache,
            "Archive",
            vec![
                rank_row(
                    20,
                    "alpha@example.com",
                    "Alpha",
                    Some(1_500),
                    Some(" <shared@example.com> "),
                    None,
                    false,
                    false,
                ),
                rank_row(
                    21,
                    "delta@example.com",
                    "Delta",
                    Some(2_500),
                    None,
                    None,
                    false,
                    false,
                ),
            ],
        )
        .await;

        let scope = test_scope(&cache, &["INBOX", "Archive"]);
        let page = query_domain_page(scope.clone(), vec!["owner@owner.example".to_string()], 0, 2)
            .await
            .expect("query first domain page");
        assert_eq!(page.total_groups, 3);
        assert_eq!(page.total_messages, 5);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].domain, "example.com");
        assert_eq!(page.items[0].count, 2);
        assert_eq!(page.items[0].sample.mailbox, "Archive");
        assert_eq!(page.items[0].sample.uid, 21);
        assert_eq!(page.items[1].domain, "mail.example.com");
        assert_eq!(page.items[1].count, 2);
        assert_eq!(page.items[1].sample.uid, 2);

        let second = query_domain_page(scope, vec!["owner@owner.example".to_string()], 2, 2)
            .await
            .expect("query second domain page");
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].domain, "xn--bcher-kva.de");
        assert_eq!(second.items[0].count, 1);
    }

    #[tokio::test]
    async fn sql_subscription_and_mailing_list_rankings_use_safe_samples() {
        let cache = HeaderCache::at_path(test_path("list-ranking"));
        publish_test_rows(
            &cache,
            "INBOX",
            vec![
                rank_row(
                    1,
                    "news@example.com",
                    "News",
                    Some(1_000),
                    Some("<news-old@example.com>"),
                    Some("Old Name <NEWS.Example.COM>"),
                    true,
                    false,
                ),
                rank_row(
                    2,
                    "news@example.com",
                    "Latest News",
                    Some(3_000),
                    Some("<news-new@example.com>"),
                    Some("Current Name <news.example.com>"),
                    true,
                    true,
                ),
                rank_row(
                    3,
                    "third@example.com",
                    "Third",
                    Some(2_000),
                    None,
                    Some("Current Name <news.example.com>"),
                    false,
                    false,
                ),
            ],
        )
        .await;
        publish_test_rows(
            &cache,
            "Archive",
            vec![
                rank_row(
                    20,
                    "news@example.com",
                    "Latest News",
                    Some(3_000),
                    Some("<news-new@example.com>"),
                    Some("Current Name <news.example.com>"),
                    true,
                    true,
                ),
                rank_row(
                    21,
                    "archive@example.com",
                    "Archive",
                    Some(4_000),
                    None,
                    Some("Newest Name <NEWS.EXAMPLE.COM>"),
                    false,
                    false,
                ),
            ],
        )
        .await;
        let scope = test_scope(&cache, &["INBOX", "Archive"]);

        let subscriptions = query_subscription_page(scope.clone(), Vec::new(), 0, 10)
            .await
            .expect("query subscriptions");
        assert_eq!(subscriptions.total_messages, 2);
        assert_eq!(subscriptions.total_groups, 1);
        let subscription = &subscriptions.items[0];
        assert_eq!(subscription.address, "news@example.com");
        assert_eq!(subscription.count, 2);
        assert!(subscription.advertised_one_click);
        assert_eq!(subscription.sample.mailbox, "INBOX");
        assert_eq!(subscription.sample.uid, 2);

        let lists = query_mailing_list_page(scope, 0, 10)
            .await
            .expect("query mailing lists");
        assert_eq!(lists.total_messages, 4);
        assert_eq!(lists.total_groups, 1);
        let list = &lists.items[0];
        assert_eq!(list.list_id, "news.example.com");
        assert_eq!(list.display_name, "Newest Name");
        assert_eq!(list.count, 4);
        assert_eq!(list.sender_count, 3);
        assert_eq!(
            list.senders,
            [
                "archive@example.com",
                "news@example.com",
                "third@example.com"
            ]
        );
        assert_eq!(list.sample.mailbox, "Archive");
        assert_eq!(list.sample.uid, 21);
    }

    #[tokio::test]
    async fn sql_ranking_page_preserves_requested_limit() {
        let cache = HeaderCache::at_path(test_path("ranking-cap"));
        let rows = (1..=105)
            .map(|uid| {
                rank_row(
                    uid,
                    &format!("sender{uid:03}@example.com"),
                    "",
                    Some(i64::from(uid)),
                    None,
                    None,
                    false,
                    false,
                )
            })
            .collect();
        publish_test_rows(&cache, "INBOX", rows).await;
        let page = query_sender_page(test_scope(&cache, &["INBOX"]), Vec::new(), 0, 7)
            .await
            .expect("query requested sender page");
        assert_eq!(page.total_groups, 105);
        assert_eq!(page.items.len(), 7);
    }

    #[cfg(unix)]
    #[test]
    fn cache_directory_and_database_are_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_path("permissions");
        drop(open_connection(&path).expect("open cache"));
        let directory_mode = std::fs::metadata(path.parent().expect("cache parent"))
            .expect("read directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&path)
            .expect("read cache metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[tokio::test]
    async fn concurrent_schema_upgrade_is_serialized() {
        let path = Arc::new(test_path("concurrent-migration"));
        let seed_path = Arc::clone(&path);
        tokio::task::spawn_blocking(move || {
            let connection = open_connection(&seed_path)?;
            // Use a destructive legacy version here; v5 and v6 have dedicated
            // additive migrations and require their corresponding schemas.
            connection.pragma_update(None, "user_version", 4)?;
            CacheResult::Ok(())
        })
        .await
        .expect("join seed migration")
        .expect("seed old schema version");

        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut migrations = Vec::new();
        for _ in 0..8 {
            let migration_path = Arc::clone(&path);
            let migration_barrier = Arc::clone(&barrier);
            migrations.push(tokio::task::spawn_blocking(move || {
                migration_barrier.wait();
                drop(open_connection(&migration_path)?);
                CacheResult::Ok(())
            }));
        }
        for migration in migrations {
            migration
                .await
                .expect("join concurrent migration")
                .expect("run concurrent migration");
        }

        let inspect_path = Arc::clone(&path);
        let version = tokio::task::spawn_blocking(move || {
            let connection = open_connection(&inspect_path)?;
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .map_err(CacheError::from)
        })
        .await
        .expect("join version inspection")
        .expect("inspect schema version");
        assert_eq!(version, CACHE_SCHEMA_VERSION);
    }

    #[test]
    fn v5_domain_migration_backfills_in_place_and_creates_covering_index() {
        let path = test_path("v5-domain-migration");
        std::fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache parent");
        {
            let connection = Connection::open(&path).expect("open v5 cache");
            connection
                .execute_batch(
                    "PRAGMA user_version = 5;
                     CREATE TABLE header_rows (
                         account_key TEXT NOT NULL,
                         mailbox TEXT NOT NULL,
                         uid_validity INTEGER NOT NULL,
                         uid INTEGER NOT NULL,
                         projection_version INTEGER NOT NULL,
                         sender_email TEXT NOT NULL,
                         sender_name TEXT NOT NULL,
                         date_unix_ms INTEGER,
                         message_id TEXT,
                         list_id TEXT,
                         list_display_name TEXT,
                         has_list_headers INTEGER NOT NULL
                             CHECK (has_list_headers IN (0, 1)),
                         advertised_one_click INTEGER NOT NULL
                             CHECK (advertised_one_click IN (0, 1)),
                         PRIMARY KEY (
                             account_key, mailbox, uid_validity, uid,
                             projection_version
                         )
                     ) WITHOUT ROWID;
                     CREATE TABLE account_quirks (
                         account_key TEXT NOT NULL PRIMARY KEY,
                         header_fields_filtered INTEGER NOT NULL DEFAULT 0
                     ) WITHOUT ROWID;
                     INSERT INTO account_quirks
                         (account_key, header_fields_filtered)
                     VALUES ('account', 1);",
                )
                .expect("create v5 schema");
            let mut insert = connection
                .prepare(
                    "INSERT INTO header_rows (
                         account_key, mailbox, uid_validity, uid,
                         projection_version, sender_email, sender_name,
                         has_list_headers, advertised_one_click
                     ) VALUES ('account', 'INBOX', 7, ?1, 5, ?2, '', 0, 0)",
                )
                .expect("prepare v5 rows");
            for (uid, address) in [
                (1_i64, "sender@MAIL.Example.COM."),
                (2, "reader@BÜCHER.DE"),
                (3, "literal@127.0.0.1"),
            ] {
                insert
                    .execute(params![uid, address])
                    .expect("insert v5 row");
            }
        }

        let connection = open_connection(&path).expect("migrate v5 cache");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated version");
        assert_eq!(version, CACHE_SCHEMA_VERSION);
        let senders_and_domains = {
            let mut statement = connection
                .prepare(
                    "SELECT uid, sender_email, sender_domain
                       FROM header_rows ORDER BY uid",
                )
                .expect("prepare migrated domains");
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .expect("query migrated domains");
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect migrated domains")
        };
        assert_eq!(
            senders_and_domains,
            [
                (
                    1,
                    "sender@mail.example.com".to_string(),
                    "mail.example.com".to_string(),
                ),
                (
                    2,
                    "reader@xn--bcher-kva.de".to_string(),
                    "xn--bcher-kva.de".to_string(),
                ),
                (3, "literal@127.0.0.1".to_string(), String::new()),
            ]
        );
        let quirk: i64 = connection
            .query_row(
                "SELECT header_fields_filtered FROM account_quirks
                  WHERE account_key = 'account'",
                [],
                |row| row.get(0),
            )
            .expect("read preserved quirk");
        assert_eq!(quirk, 1, "the additive migration preserves v5 state");
        let index_exists: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'index' AND name = 'header_rows_by_domain'",
                [],
                |row| row.get(0),
            )
            .expect("inspect domain index");
        assert_eq!(index_exists, 1);
        let index_columns = {
            let mut statement = connection
                .prepare("PRAGMA index_info(header_rows_by_domain)")
                .expect("prepare domain index inspection");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(2))
                .expect("query domain index columns");
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect domain index columns")
        };
        assert_eq!(
            index_columns,
            [
                "account_key",
                "mailbox",
                "uid_validity",
                "projection_version",
                "sender_domain",
                "uid",
            ]
        );
    }

    #[test]
    fn v6_migration_canonicalizes_idn_sender_without_readding_domain_column() {
        let path = test_path("v6-idn-migration");
        std::fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache parent");
        {
            let connection = Connection::open(&path).expect("open v6 cache");
            connection
                .execute_batch(
                    "PRAGMA user_version = 6;
                     CREATE TABLE header_rows (
                         account_key TEXT NOT NULL,
                         mailbox TEXT NOT NULL,
                         uid_validity INTEGER NOT NULL,
                         uid INTEGER NOT NULL,
                         projection_version INTEGER NOT NULL,
                         sender_email TEXT NOT NULL,
                         sender_domain TEXT NOT NULL,
                         sender_name TEXT NOT NULL,
                         date_unix_ms INTEGER,
                         message_id TEXT,
                         list_id TEXT,
                         list_display_name TEXT,
                         has_list_headers INTEGER NOT NULL
                             CHECK (has_list_headers IN (0, 1)),
                         advertised_one_click INTEGER NOT NULL
                             CHECK (advertised_one_click IN (0, 1)),
                         PRIMARY KEY (
                             account_key, mailbox, uid_validity, uid,
                             projection_version
                         )
                     ) WITHOUT ROWID;
                     INSERT INTO header_rows (
                         account_key, mailbox, uid_validity, uid,
                         projection_version, sender_email, sender_domain,
                         sender_name, has_list_headers, advertised_one_click
                     ) VALUES (
                         'account', 'INBOX', 7, 1, 5,
                         'Reader@BÜCHER.DE', 'xn--bcher-kva.de', '', 0, 0
                     );",
                )
                .expect("create v6 schema");
        }

        let connection = open_connection(&path).expect("migrate v6 cache");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated version");
        let (email, domain): (String, String) = connection
            .query_row(
                "SELECT sender_email, sender_domain FROM header_rows",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read migrated row");
        assert_eq!(version, CACHE_SCHEMA_VERSION);
        assert_eq!(email, "reader@xn--bcher-kva.de");
        assert_eq!(domain, "xn--bcher-kva.de");
    }

    #[test]
    fn v2_token_columns_are_rebuilt_and_checkpointed() {
        let path = test_path("v2-token-migration");
        std::fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache parent");
        let secret = "https://unsubscribe.example.test/private-recipient-token-928374";
        {
            let connection = Connection::open(&path).expect("open legacy cache");
            connection
                .execute_batch(
                    "PRAGMA user_version = 2;
                     CREATE TABLE header_rows (
                         account_key TEXT NOT NULL,
                         mailbox TEXT NOT NULL,
                         uid_validity INTEGER NOT NULL,
                         uid INTEGER NOT NULL,
                         projection_version INTEGER NOT NULL,
                         sender_email TEXT NOT NULL,
                         sender_name TEXT NOT NULL,
                         date_unix_ms INTEGER,
                         message_id TEXT,
                         list_id TEXT,
                         list_unsubscribe TEXT,
                         list_unsubscribe_post TEXT,
                         PRIMARY KEY (
                             account_key, mailbox, uid_validity, uid,
                             projection_version
                         )
                     ) WITHOUT ROWID;",
                )
                .expect("create legacy schema");
            connection
                .execute(
                    "INSERT INTO header_rows (
                         account_key, mailbox, uid_validity, uid,
                         projection_version, sender_email, sender_name,
                         list_unsubscribe
                     ) VALUES ('account', 'INBOX', 7, 1, 2,
                               'sender@example.com', '', ?1)",
                    params![secret],
                )
                .expect("insert legacy token");
        }

        let connection = open_connection(&path).expect("migrate legacy cache");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read migrated version");
        assert_eq!(version, CACHE_SCHEMA_VERSION);
        let columns = {
            let mut statement = connection
                .prepare("PRAGMA table_info(header_rows)")
                .expect("prepare schema query");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(1))
                .expect("query schema");
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect schema")
        };
        assert!(!columns.iter().any(|column| column == "list_unsubscribe"));
        assert!(
            columns
                .iter()
                .any(|column| column == "advertised_one_click")
        );
        drop(connection);

        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
        ] {
            if candidate.exists() {
                let bytes = std::fs::read(&candidate).expect("read migrated cache file");
                assert!(
                    !bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes()),
                    "legacy unsubscribe token remained in {}",
                    candidate.display()
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "explicit 216K-message SQLite stress test"]
    async fn large_mailbox_snapshot_uses_bounded_chunk_writes() {
        const MESSAGE_COUNT: u32 = 216_000;

        let cache = HeaderCache::at_path(test_path("216k-stress"));
        let key = test_key("All Mail");
        let path = Arc::clone(cache.path.as_ref().expect("test cache path"));
        let uids: Vec<u32> = (1..=MESSAGE_COUNT).collect();
        for chunk in uids.chunks(FETCH_CHUNK_SIZE) {
            store_headers(
                path.clone(),
                key.clone(),
                7,
                chunk.iter().copied().map(test_row).collect(),
            )
            .await
            .expect("store header chunk");
        }

        let outcome = publish_snapshot(
            path.clone(),
            key.clone(),
            None,
            0,
            7,
            Some(MESSAGE_COUNT + 1),
            &uids,
            None,
        )
        .await
        .expect("publish large snapshot");
        assert_eq!(outcome, PublishOutcome::Published);
        let state = load_state(path.clone(), key.clone())
            .await
            .expect("load state")
            .expect("published state");
        assert_eq!(state.exists, MESSAGE_COUNT);
        let page = query_sender_page(
            RankScope {
                path,
                account: key.account,
                mailboxes: vec![key.mailbox],
            },
            Vec::new(),
            0,
            10,
        )
        .await
        .expect("query bounded page");
        assert_eq!(page.total_messages, u64::from(MESSAGE_COUNT));
        assert_eq!(page.total_groups, u64::from(MESSAGE_COUNT));
        assert_eq!(page.items.len(), 10);
    }
}
