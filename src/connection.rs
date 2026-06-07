use hashbrown::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::AgentmailError;
use crate::config::{AccountConfig, Config};
use crate::imap_client::{self, ImapSession};

/// An idle session returned to the pool, tagged with when it went idle so we can
/// evict ones the IMAP server has very likely already dropped.
struct IdleSession {
    session: ImapSession,
    idle_since: Instant,
}

/// Connection pool managing IMAP sessions across accounts.
pub struct ConnectionPool {
    config: Config,
    /// Per-account pool of idle sessions.
    pools: Arc<Mutex<HashMap<String, Vec<IdleSession>>>>,
    /// Per-account semaphores to cap concurrent IMAP operations.
    semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

/// Max concurrent IMAP operations per account.
/// Most IMAP servers allow 10-15 connections; we stay well under that.
const MAX_CONCURRENT_PER_ACCOUNT: usize = 3;

/// Don't reuse a pooled session idle longer than this. IMAP servers typically
/// drop idle connections after ~30 min; past this threshold the cached session
/// is very likely dead, so reconnecting fresh (~1-2s) beats paying a ~15s
/// dead-`NOOP` ping before reconnecting anyway. Well under the server timeout.
const MAX_IDLE: Duration = Duration::from_secs(5 * 60);

/// Whether a session idle for `idle_for` is fresh enough to attempt reuse.
fn idle_is_fresh(idle_for: Duration) -> bool {
    idle_for < MAX_IDLE
}

impl ConnectionPool {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            pools: Arc::new(Mutex::new(HashMap::new())),
            semaphores: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get or create the semaphore for a given account.
    /// Uses the account's `max_connections` config if set, otherwise the default.
    async fn account_semaphore(&self, account_name: &str) -> Arc<Semaphore> {
        let mut sems = self.semaphores.lock().await;
        sems.entry(account_name.to_string())
            .or_insert_with(|| {
                let limit = self
                    .config
                    .accounts
                    .get(account_name)
                    .and_then(|c| c.max_connections)
                    .unwrap_or(MAX_CONCURRENT_PER_ACCOUNT);
                Arc::new(Semaphore::new(limit))
            })
            .clone()
    }

    /// Get the max connections limit for an account.
    fn account_max_connections(&self, account_name: &str) -> usize {
        self.config
            .accounts
            .get(account_name)
            .and_then(|c| c.max_connections)
            .unwrap_or(MAX_CONCURRENT_PER_ACCOUNT)
    }

    /// Acquire a session for the named account.
    /// Blocks if the per-account concurrency limit is reached.
    pub async fn acquire(&self, account_name: &str) -> crate::Result<PooledSession> {
        let account_config = self
            .config
            .accounts
            .get(account_name)
            .ok_or_else(|| AgentmailError::AccountNotFound(account_name.to_string()))?;

        let max_conn = self.account_max_connections(account_name);

        // Acquire a concurrency permit (blocks if at cap)
        let sem = self.account_semaphore(account_name).await;
        let permit = sem
            .acquire_owned()
            .await
            .map_err(|_| AgentmailError::Other("concurrency semaphore closed".to_string()))?;

        // Pop a candidate idle session while holding the lock briefly
        let maybe_idle = {
            let mut pools = self.pools.lock().await;
            pools.get_mut(account_name).and_then(|pool| pool.pop())
        }; // lock released here — before any network I/O

        // Reuse it only if it hasn't been idle long enough for the server to
        // have dropped it AND it still answers a NOOP. Otherwise drop it and
        // reconnect fresh below. The age check is what keeps AgentMail working
        // after a long idle: a session idle past MAX_IDLE is very likely dead,
        // so we skip the ~15s dead-`NOOP` ping and reconnect straight away.
        if let Some(idle) = maybe_idle {
            let mut session = idle.session;
            if idle_is_fresh(idle.idle_since.elapsed())
                && imap_client::ping(&mut session).await.is_ok()
            {
                return Ok(PooledSession {
                    session: Some(session),
                    account_name: account_name.to_string(),
                    pool: Arc::clone(&self.pools),
                    max_connections: max_conn,
                    _permit: permit,
                });
            }
            // too old or stale → `session` drops here (connection closes)
        }
        // No reusable session — create fresh

        // Create new connection
        let password = crate::credentials::get_password(account_name, account_config).await?;
        let session = imap_client::connect(account_config, &password).await?;

        Ok(PooledSession {
            session: Some(session),
            account_name: account_name.to_string(),
            pool: Arc::clone(&self.pools),
            max_connections: max_conn,
            _permit: permit,
        })
    }

    /// Run an IMAP operation against a pooled session, retrying it **once** with
    /// a fresh connection if it fails with a connection-level error (the socket
    /// died mid-operation — after `acquire`'s liveness ping passed). The dead
    /// session is dropped, not returned to the pool. Server rejections, parse
    /// errors, etc. are returned as-is (no retry). This closes the
    /// connection-died-*during*-an-op race that the idle-TTL eviction can't see.
    pub async fn with_session_retry<T>(
        &self,
        account: &str,
        mut op: impl AsyncFnMut(&mut ImapSession) -> crate::Result<T>,
    ) -> crate::Result<T> {
        let mut session = self.acquire(account).await?;
        match op(session.session()).await {
            Err(e) if e.is_connection_error() => {
                drop(session); // dead — don't hand it back to the pool
                tracing::warn!(
                    target: "agentmail",
                    "IMAP connection dropped mid-operation for {account}; retrying once with a fresh connection: {e}",
                );
                let mut fresh = self.acquire(account).await?;
                let result = op(fresh.session()).await;
                fresh.release().await;
                result
            }
            other => {
                session.release().await;
                other
            }
        }
    }

    /// List all configured account names.
    pub fn account_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.config.accounts.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get the account config for a named account.
    pub fn account_config(&self, name: &str) -> Option<&AccountConfig> {
        self.config.accounts.get(name)
    }

    /// Get the underlying config.
    pub fn config(&self) -> &Config {
        &self.config
    }
}

/// A session borrowed from the pool. Must be explicitly released or dropped.
/// Holds a semaphore permit that is released when the session is returned or dropped,
/// allowing the next queued operation to proceed.
pub struct PooledSession {
    session: Option<ImapSession>,
    account_name: String,
    pool: Arc<Mutex<HashMap<String, Vec<IdleSession>>>>,
    /// Max idle sessions to keep for this account.
    max_connections: usize,
    /// Concurrency permit — released on drop to unblock waiting callers.
    _permit: OwnedSemaphorePermit,
}

impl PooledSession {
    /// Get a mutable reference to the underlying IMAP session.
    pub fn session(&mut self) -> &mut ImapSession {
        self.session.as_mut().expect("session already consumed")
    }

    /// Return the session to the pool for reuse.
    /// The concurrency permit is released when this PooledSession is dropped.
    pub async fn release(mut self) {
        if let Some(session) = self.session.take() {
            let mut pools = self.pool.lock().await;
            let pool = pools.entry(self.account_name.clone()).or_default();
            if pool.len() < self.max_connections {
                pool.push(IdleSession {
                    session,
                    idle_since: Instant::now(),
                });
            }
            // else: drop the session (connection closes)
        }
        // self is dropped here → _permit is dropped → semaphore slot freed
    }
}

impl Drop for PooledSession {
    fn drop(&mut self) {
        // If release() wasn't called, the session is simply dropped (connection closes).
        // The _permit is also dropped here, freeing the semaphore slot.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A just-released session is reusable; one idle past `MAX_IDLE` is evicted
    /// (reconnect fresh instead of pinging a likely-dead connection).
    #[test]
    fn idle_freshness_threshold() {
        assert!(
            idle_is_fresh(Duration::from_secs(0)),
            "fresh session reusable"
        );
        assert!(
            idle_is_fresh(MAX_IDLE - Duration::from_secs(1)),
            "just under the threshold is still reusable",
        );
        assert!(
            !idle_is_fresh(MAX_IDLE),
            "exactly at the threshold is evicted"
        );
        assert!(
            !idle_is_fresh(MAX_IDLE + Duration::from_secs(30 * 60)),
            "long-idle (likely server-dropped) session is evicted",
        );
    }
}
