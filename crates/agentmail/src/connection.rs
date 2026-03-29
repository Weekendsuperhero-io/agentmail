use hashbrown::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::AgentmailError;
use crate::config::{AccountConfig, Config};
use crate::imap_client::{self, ImapSession};

/// Sessions idle longer than this are discarded without pinging.
/// Gmail drops idle IMAP connections after ~10 minutes.
const MAX_IDLE_SECS: u64 = 120;

/// An idle session with the time it was returned to the pool.
struct IdleSession {
    session: ImapSession,
    returned_at: Instant,
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

        // Pop a candidate session while holding the lock briefly
        let maybe_idle = {
            let mut pools = self.pools.lock().await;
            pools.get_mut(account_name).and_then(|pool| pool.pop())
        }; // lock released here — before any network I/O

        // Validate the candidate outside the lock
        if let Some(idle) = maybe_idle {
            let age = idle.returned_at.elapsed().as_secs();
            if age < MAX_IDLE_SECS {
                // Recent enough — ping to verify it's alive
                let mut session = idle.session;
                if imap_client::ping(&mut session).await.is_ok() {
                    return Ok(PooledSession {
                        session: Some(session),
                        account_name: account_name.to_string(),
                        pool: Arc::clone(&self.pools),
                        max_connections: max_conn,
                        _permit: permit,
                    });
                }
                tracing::debug!("Pooled IMAP session for {} failed ping, creating fresh", account_name);
            } else {
                tracing::debug!("Discarding IMAP session for {} (idle {}s > {}s)", account_name, age, MAX_IDLE_SECS);
            }
        }
        // Session was stale, too old, or absent — create fresh

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

    /// Run an operation with automatic retry on connection failure.
    /// If the operation fails with an IMAP/IO/timeout error, acquires a fresh
    /// session and retries once. This handles idle-timeout disconnects transparently.
    pub async fn with_retry<F, Fut, T>(
        &self,
        account_name: &str,
        op: F,
    ) -> crate::Result<T>
    where
        F: Fn(PooledSession) -> Fut + Send,
        Fut: std::future::Future<Output = (PooledSession, crate::Result<T>)> + Send,
    {
        let session = self.acquire(account_name).await?;
        let (session, result) = op(session).await;
        match result {
            Ok(val) => {
                session.release().await;
                Ok(val)
            }
            Err(ref e) if Self::is_connection_error(e) => {
                tracing::warn!("IMAP operation failed ({}), retrying with fresh session", e);
                drop(session); // drop the stale session
                let retry_session = self.acquire(account_name).await?;
                let (retry_session, retry_result) = op(retry_session).await;
                if retry_result.is_ok() {
                    retry_session.release().await;
                } else {
                    drop(retry_session);
                }
                retry_result
            }
            Err(e) => {
                drop(session);
                Err(e)
            }
        }
    }

    /// Check if an error likely indicates a dead/stale IMAP connection.
    fn is_connection_error(e: &crate::AgentmailError) -> bool {
        matches!(
            e,
            crate::AgentmailError::Imap(_)
                | crate::AgentmailError::Io(_)
                | crate::AgentmailError::NotConnected
        ) || matches!(e, crate::AgentmailError::Other(msg) if msg.contains("timed out"))
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
                    returned_at: Instant::now(),
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
