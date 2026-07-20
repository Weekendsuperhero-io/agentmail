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
    /// Per-account server capabilities. Capabilities describe the server, not
    /// the socket, so one CAPABILITY round trip per process per account
    /// suffices (a server upgraded mid-process only changes which command
    /// variant we pick — harmless).
    caps: Arc<parking_lot::Mutex<HashMap<String, Arc<imap_client::ServerCaps>>>>,
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

/// Drive one acquire→run→release cycle, retrying **once** with a freshly
/// acquired handle when the operation fails with a connection-level error.
/// Shared by [`ConnectionPool::with_session_retry`] and its unit tests so the
/// control flow is testable without a real IMAP session:
/// - `Ok` or a non-connection error on the first attempt → handle released,
///   result returned (no retry).
/// - Connection error on the first attempt → the dead handle is dropped (never
///   released) and the op runs exactly once more on a fresh handle.
/// - The second result is returned as-is; the second handle is released unless
///   it also died with a connection error, in which case it is dropped too.
///
/// This is a macro rather than a generic `async fn`: a generic helper nests
/// the caller's `AsyncFnMut` call-future inside another opaque future, which
/// makes its `Send` obligation higher-ranked and unsolvable at the MCP tool
/// boundaries ("implementation of `Send` is not general enough"). Expanding
/// inline keeps every type concrete, exactly like handwritten code.
///
/// `$acquire` is a closure returning a `crate::Result<H>` future, `$session_of`
/// projects `&mut H` to the value handed to `$op`, and `$release` consumes an
/// `H`. Expands to a `crate::Result<T>` expression; errors are yielded, not
/// `?`-propagated, so tests can observe them.
macro_rules! retry_once {
    ($acquire:expr, $session_of:expr, $op:expr, $release:expr $(,)?) => {{
        #[allow(unused_mut)]
        let mut acquire = $acquire;
        #[allow(unused_mut)]
        let mut session_of = $session_of;
        #[allow(unused_mut)]
        let mut op = $op;
        #[allow(unused_mut)]
        let mut release = $release;
        match acquire().await {
            Err(e) => Err(e),
            Ok(mut handle) => {
                // Bound as a `let` so the scrutinee's `&mut handle` borrow ends
                // here — in `match op(...)` scrutinee position it would be
                // extended to the end of the match, forbidding `drop(handle)`.
                let first = op(session_of(&mut handle)).await;
                match first {
                    Err(e) if e.is_connection_error() => {
                        drop(handle); // dead — don't hand it back to the pool
                        tracing::warn!(
                            target: "agentmail",
                            retry = 1,
                            "IMAP connection dropped mid-operation; retrying with a fresh connection",
                        );
                        match acquire().await {
                            Err(e) => Err(e),
                            Ok(mut fresh) => {
                                let result = op(session_of(&mut fresh)).await;
                                match &result {
                                    // The retry died too — drop this handle as
                                    // well rather than returning a dead session
                                    // to the pool, where a later acquire would
                                    // pay a dead-NOOP ping for it.
                                    Err(e) if e.is_connection_error() => drop(fresh),
                                    _ => release(fresh).await,
                                }
                                result
                            }
                        }
                    }
                    other => {
                        release(handle).await;
                        other
                    }
                }
            }
        }
    }};
}

impl ConnectionPool {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            pools: Arc::new(Mutex::new(HashMap::new())),
            semaphores: Arc::new(Mutex::new(HashMap::new())),
            caps: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    /// Get the server capabilities for an account, fetching once and caching.
    /// `session` must be a live session for the account.
    pub async fn server_caps(
        &self,
        account_name: &str,
        session: &mut ImapSession,
    ) -> crate::Result<Arc<imap_client::ServerCaps>> {
        // Fast path: cached. Scoped guard — never held across the await below
        // (`await_holding_lock` is denied).
        if let Some(caps) = self.caps.lock().get(account_name).cloned() {
            return Ok(caps);
        }
        let caps = Arc::new(imap_client::ServerCaps::fetch(session).await?);
        self.caps
            .lock()
            .insert(account_name.to_string(), Arc::clone(&caps));
        Ok(caps)
    }

    /// Return cached capabilities without a session, if already fetched.
    pub fn cached_caps(&self, account_name: &str) -> Option<Arc<imap_client::ServerCaps>> {
        self.caps.lock().get(account_name).cloned()
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
    /// died mid-operation — after `acquire`'s liveness ping passed). A dead
    /// session — first or retry attempt — is dropped, never returned to the
    /// pool. Server rejections, parse errors, etc. are returned as-is (no
    /// retry). This closes the connection-died-*during*-an-op race that the
    /// idle-TTL eviction can't see. Control flow lives in the `retry_once!`
    /// macro, which is unit-tested without real sessions.
    pub async fn with_session_retry<T>(
        &self,
        account: &str,
        op: impl AsyncFnMut(&mut ImapSession) -> crate::Result<T>,
    ) -> crate::Result<T> {
        retry_once!(
            || self.acquire(account),
            PooledSession::session,
            op,
            PooledSession::release,
        )
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
    use std::sync::atomic::{AtomicBool, Ordering};

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

    /// Handle whose release-vs-drop fate is observable: the flag flips only if
    /// the release path ran; a plain drop leaves it false.
    struct TestHandle {
        released: Arc<AtomicBool>,
    }

    /// Stand-in for `PooledSession::session`: the test handle is its own
    /// "session".
    fn test_session(handle: &mut TestHandle) -> &mut TestHandle {
        handle
    }

    /// Forces a test op closure to be inferred higher-ranked over the session
    /// borrow — the same shape `with_session_retry`'s declared bound gives real
    /// ops. Without this, the closure gets one specific region shared by both
    /// attempts and borrowck rejects dropping the first handle.
    fn hr_op<T, F>(op: F) -> F
    where
        F: AsyncFnMut(&mut TestHandle) -> crate::Result<T>,
    {
        op
    }

    fn release_flags(n: usize) -> Vec<Arc<AtomicBool>> {
        (0..n).map(|_| Arc::new(AtomicBool::new(false))).collect()
    }

    fn dead_socket() -> AgentmailError {
        AgentmailError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "socket died",
        ))
    }

    /// A connection-dropped first attempt is retried exactly once on a fresh
    /// handle; the dead handle is dropped while the fresh one is released.
    #[tokio::test]
    async fn retry_once_retries_connection_error_on_fresh_handle() {
        let flags = release_flags(2);
        let mut acquired = 0;
        let mut calls = 0;

        let result = retry_once!(
            async || {
                acquired += 1;
                Ok(TestHandle {
                    released: Arc::clone(&flags[acquired - 1]),
                })
            },
            test_session,
            hr_op(async |_session| {
                calls += 1;
                if calls == 1 {
                    Err(dead_socket())
                } else {
                    Ok(42)
                }
            }),
            async |handle: TestHandle| handle.released.store(true, Ordering::SeqCst),
        );

        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls, 2, "op runs once per attempt, exactly twice");
        assert!(
            !flags[0].load(Ordering::SeqCst),
            "dead first handle is dropped, not returned to the pool"
        );
        assert!(
            flags[1].load(Ordering::SeqCst),
            "healthy second handle is released back to the pool"
        );
    }

    /// Server rejections, parse failures, etc. are not retried; the session is
    /// healthy, so it goes back to the pool.
    #[tokio::test]
    async fn retry_once_returns_non_connection_error_without_retry() {
        let flags = release_flags(1);
        let mut calls = 0;

        let result: crate::Result<()> = retry_once!(
            async || {
                Ok(TestHandle {
                    released: Arc::clone(&flags[0]),
                })
            },
            test_session,
            hr_op(async |_session| {
                calls += 1;
                Err(AgentmailError::Parse("malformed".to_string()))
            }),
            async |handle: TestHandle| handle.released.store(true, Ordering::SeqCst),
        );

        assert!(matches!(result, Err(AgentmailError::Parse(_))));
        assert_eq!(calls, 1, "non-connection errors abort without a retry");
        assert!(
            flags[0].load(Ordering::SeqCst),
            "healthy session is released back to the pool"
        );
    }

    /// The success path releases the session for reuse.
    #[tokio::test]
    async fn retry_once_releases_handle_after_success() {
        let flags = release_flags(1);
        let mut calls = 0;

        let result = retry_once!(
            async || {
                Ok(TestHandle {
                    released: Arc::clone(&flags[0]),
                })
            },
            test_session,
            hr_op(async |_session| {
                calls += 1;
                Ok("done")
            }),
            async |handle: TestHandle| handle.released.store(true, Ordering::SeqCst),
        );

        assert_eq!(result.unwrap(), "done");
        assert_eq!(calls, 1);
        assert!(
            flags[0].load(Ordering::SeqCst),
            "session is released after a successful op"
        );
    }

    /// If the retry also dies with a connection error, that session is dropped
    /// too — a dead session must never re-enter the pool, where a later
    /// `acquire` would pay a dead-NOOP ping for it.
    #[tokio::test]
    async fn retry_once_drops_second_dead_handle_instead_of_releasing() {
        let flags = release_flags(2);
        let mut acquired = 0;
        let mut calls = 0;

        let result: crate::Result<()> = retry_once!(
            async || {
                acquired += 1;
                Ok(TestHandle {
                    released: Arc::clone(&flags[acquired - 1]),
                })
            },
            test_session,
            hr_op(async |_session| {
                calls += 1;
                Err(dead_socket())
            }),
            async |handle: TestHandle| handle.released.store(true, Ordering::SeqCst),
        );

        assert!(matches!(&result, Err(e) if e.is_connection_error()));
        assert_eq!(calls, 2, "exactly one retry — never a third attempt");
        assert!(
            !flags[0].load(Ordering::SeqCst) && !flags[1].load(Ordering::SeqCst),
            "both dead handles are dropped, not returned to the pool"
        );
    }

    /// A failure to reconnect for the retry propagates as-is.
    #[tokio::test]
    async fn retry_once_propagates_reacquire_failure() {
        let flags = release_flags(1);
        let mut acquired = 0;
        let mut calls = 0;

        let result: crate::Result<()> = retry_once!(
            async || {
                acquired += 1;
                if acquired == 1 {
                    Ok(TestHandle {
                        released: Arc::clone(&flags[0]),
                    })
                } else {
                    Err(AgentmailError::AccountNotFound("gone".to_string()))
                }
            },
            test_session,
            hr_op(async |_session| {
                calls += 1;
                Err(dead_socket())
            }),
            async |handle: TestHandle| handle.released.store(true, Ordering::SeqCst),
        );

        assert!(matches!(result, Err(AgentmailError::AccountNotFound(_))));
        assert_eq!(calls, 1, "op does not run again without a fresh session");
        assert!(
            !flags[0].load(Ordering::SeqCst),
            "the dead first handle is dropped"
        );
    }
}
