use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::MailkitError;
use crate::config::{AccountConfig, Config};
use crate::imap_client::{self, ImapSession};

/// Connection pool managing IMAP sessions across accounts.
pub struct ConnectionPool {
    config: Config,
    /// Per-account pool of idle sessions.
    pools: Arc<Mutex<HashMap<String, Vec<ImapSession>>>>,
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
    async fn account_semaphore(&self, account_name: &str) -> Arc<Semaphore> {
        let mut sems = self.semaphores.lock().await;
        sems.entry(account_name.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_CONCURRENT_PER_ACCOUNT)))
            .clone()
    }

    /// Acquire a session for the named account.
    /// Blocks if the per-account concurrency limit is reached.
    pub async fn acquire(&self, account_name: &str) -> crate::Result<PooledSession> {
        let account_config = self
            .config
            .accounts
            .get(account_name)
            .ok_or_else(|| MailkitError::AccountNotFound(account_name.to_string()))?;

        // Acquire a concurrency permit (blocks if at cap)
        let sem = self.account_semaphore(account_name).await;
        let permit = sem
            .acquire_owned()
            .await
            .map_err(|_| MailkitError::Other("concurrency semaphore closed".to_string()))?;

        // Pop a candidate session while holding the lock briefly
        let maybe_session = {
            let mut pools = self.pools.lock().await;
            pools.get_mut(account_name).and_then(|pool| pool.pop())
        }; // lock released here — before any network I/O

        // Validate the candidate outside the lock
        if let Some(mut session) = maybe_session
            && imap_client::ping(&mut session).await.is_ok()
        {
            return Ok(PooledSession {
                session: Some(session),
                account_name: account_name.to_string(),
                pool: Arc::clone(&self.pools),
                _permit: permit,
            });
        }
        // Session was stale (or absent), drop it and create fresh

        // Create new connection
        let password = crate::credentials::get_password(account_name, account_config).await?;
        let session = imap_client::connect(account_config, &password).await?;

        Ok(PooledSession {
            session: Some(session),
            account_name: account_name.to_string(),
            pool: Arc::clone(&self.pools),
            _permit: permit,
        })
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
    pool: Arc<Mutex<HashMap<String, Vec<ImapSession>>>>,
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
            if pool.len() < MAX_CONCURRENT_PER_ACCOUNT {
                pool.push(session);
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
