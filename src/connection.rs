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
    /// Per-account pool of idle RFC 9586 UID-Mode sessions, kept separate
    /// because `ENABLE UIDONLY` is sticky for the connection's life: a
    /// UID-Mode session must never serve a Limited-Mode operation, but
    /// reusing one for the next ranking/sweep call skips a whole LOGIN +
    /// ENABLE — the difference between one login per process and one login
    /// per tool call on rate-limited providers (AOL/Yahoo).
    uid_pools: Arc<Mutex<HashMap<String, Vec<IdleSession>>>>,
    /// Per-account semaphores to cap concurrent IMAP operations.
    semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    /// Per-account server capabilities. Capabilities describe the server, not
    /// the socket, so one CAPABILITY round trip per process per account
    /// suffices (a server upgraded mid-process only changes which command
    /// variant we pick — harmless).
    caps: Arc<parking_lot::Mutex<HashMap<String, Arc<imap_client::ServerCaps>>>>,
    /// Per-account login-rate-limit state ([`LoginCooldown`]): while armed,
    /// NEW logins are refused (every further attempt is another LOGIN strike
    /// that extends the server-side penalty); idle pooled sessions keep
    /// working (reuse ≠ LOGIN). Consecutive LIMITs escalate the window;
    /// expired entries are kept as the consecutiveness memory and cleared
    /// only by a successful fresh login.
    login_cooldowns: Arc<parking_lot::Mutex<HashMap<String, LoginCooldown>>>,
    /// BASE cooldown for the first LIMIT of an episode; consecutive LIMITs
    /// double it up to [`LOGIN_RATE_LIMIT_COOLDOWN_CAP`]. Defaults to
    /// [`LOGIN_RATE_LIMIT_COOLDOWN`]; embedding apps tune it via
    /// `Agentmail::builder(..).login_cooldown(..)`.
    login_cooldown: Duration,
    /// Per-account connect singleflight: at most one task per account runs
    /// `imap_client::connect` (a LOGIN) at a time; queued waiters re-check
    /// the idle pool and the cooldown gate before attempting their own —
    /// serialized — connect. The entry lock is tokio's because it is held
    /// across the connect await; the OUTER map lock is parking_lot precisely
    /// so the denied `await_holding_lock` lint mechanically forbids holding
    /// the map guard across the entry lock's `.lock().await`.
    connect_locks: Arc<parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Reuse-eligibility threshold for idle sessions. Defaults to
    /// [`MAX_IDLE`]; embedding apps raise it (with `keepalive` or a
    /// provider whose server timeout is known) via
    /// `Agentmail::builder(..).max_idle(..)`.
    max_idle: Duration,
    /// When set, a background task NOOPs every idle session (Limited and
    /// UID-Mode pools) on this interval so none go stale — a few LOGINs per
    /// process instead of one per gap in traffic. Opt-in via
    /// `Agentmail::builder(..).keepalive(..)`.
    keepalive: Option<Duration>,
    /// The running keepalive task, spawned lazily on first pool use (an async
    /// context, so a runtime is guaranteed) and aborted when the pool drops.
    keepalive_task: Arc<parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

/// Max concurrent IMAP operations per account.
/// Most IMAP servers allow 10-15 connections; we stay well under that.
const MAX_CONCURRENT_PER_ACCOUNT: usize = 3;

/// Don't reuse a pooled session idle longer than this. IMAP servers typically
/// drop idle connections after ~30 min; past this threshold the cached session
/// is very likely dead, so reconnecting fresh (~1-2s) beats paying a ~15s
/// dead-`NOOP` ping before reconnecting anyway. Well under the server timeout.
const MAX_IDLE: Duration = Duration::from_secs(5 * 60);

/// How long to refuse new logins for an account after the server rate-limited
/// LOGIN. AOL/Yahoo do not advertise the penalty window; five minutes is long
/// enough for it to clear and short enough not to strand a recovered account.
const LOGIN_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(300);

/// Max idle UID-Mode sessions to keep per account. Ranking scans and delete
/// sweeps run one at a time in practice; extra idle UID sessions would only
/// occupy server connection slots.
const MAX_IDLE_UID_MODE: usize = 1;

/// Floor for the configurable keepalive interval: NOOPs more often than this
/// are pure wire chatter with no liveness benefit.
const MIN_KEEPALIVE: Duration = Duration::from_secs(30);

/// Whether a session idle for `idle_for` is fresh enough to attempt reuse
/// against the pool's configured threshold.
fn idle_is_fresh(idle_for: Duration, max_idle: Duration) -> bool {
    idle_for < max_idle
}

/// Ceiling for the escalating login cooldown. One hour outlasts observed
/// AOL/Yahoo LOGIN penalty windows; doubling past it would only strand a
/// recovered account.
const LOGIN_RATE_LIMIT_COOLDOWN_CAP: Duration = Duration::from_secs(3600);

/// Per-account login-rate-limit state: when the gate lifts and how many
/// consecutive LIMITs led here. Strikes drive the escalating cooldown —
/// expired entries are deliberately KEPT in the map, because a lapsed entry
/// is the memory that decides whether the next LIMIT counts as consecutive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoginCooldown {
    /// New LOGINs are refused until this instant.
    until: Instant,
    /// Consecutive LIMIT strikes; 1 = first (or lapsed-episode) LIMIT.
    strikes: u32,
}

/// Time left on a login cooldown that ends at `until`, `None` once it lifted.
fn cooldown_remaining(until: Instant, now: Instant) -> Option<Duration> {
    (until > now).then(|| until - now)
}

/// Cooldown applied at the given strike count: the base doubled per extra
/// strike, capped. The cap honors an over-cap builder base rather than
/// silently shrinking it. Defensive on `strikes == 0` (treated as 1) and
/// overflow-safe via exponent clamp plus saturating multiply.
fn cooldown_after_strikes(base: Duration, strikes: u32) -> Duration {
    let cap = LOGIN_RATE_LIMIT_COOLDOWN_CAP.max(base);
    let factor = 1u32 << strikes.max(1).saturating_sub(1).min(31);
    base.saturating_mul(factor).min(cap)
}

/// State after one more LIMIT at `now`. Consecutive = the new LIMIT lands
/// before the previous cooldown's expiry plus a grace window of 2× its
/// length — covering both a LIMIT while still armed and one shortly after
/// expiry (the "server penalty outlasts our cooldown" case, where each
/// expiry's single probing LOGIN gets re-LIMITed). At or past the window
/// boundary the episode has lapsed and the account starts over at strike 1.
fn next_cooldown(prev: Option<LoginCooldown>, base: Duration, now: Instant) -> LoginCooldown {
    let strikes = match prev {
        Some(prev)
            if now < prev.until + cooldown_after_strikes(base, prev.strikes).saturating_mul(2) =>
        {
            prev.strikes.saturating_add(1)
        }
        _ => 1,
    };
    LoginCooldown {
        until: now + cooldown_after_strikes(base, strikes),
        strikes,
    }
}

/// The strike-aware fast-fail error for a gated account.
fn cooldown_error(account_name: &str, remaining: Duration, strikes: u32) -> AgentmailError {
    AgentmailError::Other(format!(
        "{account_name}: the server rate-limited LOGIN; refusing new connections for another {}s (strike {strikes}; the cooldown doubles up to 60m while LIMITs continue — pooled sessions keep working). Retry after the pause.",
        remaining.as_secs().max(1)
    ))
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
            uid_pools: Arc::new(Mutex::new(HashMap::new())),
            semaphores: Arc::new(Mutex::new(HashMap::new())),
            caps: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            login_cooldowns: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            login_cooldown: LOGIN_RATE_LIMIT_COOLDOWN,
            connect_locks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            max_idle: MAX_IDLE,
            keepalive: None,
            keepalive_task: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// Get or create the connect-singleflight lock for an account. Sync; the
    /// map guard is scoped to this fn — callers await the returned entry
    /// lock only after this guard has dropped.
    fn account_connect_lock(&self, account_name: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.connect_locks
                .lock()
                .entry(account_name.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Override how long the login-rate-limit cooldown lasts (builder use).
    /// Floored at 1s so the gate cannot be configured away entirely.
    pub(crate) fn set_login_cooldown(&mut self, cooldown: Duration) {
        self.login_cooldown = cooldown.max(Duration::from_secs(1));
    }

    /// Override the idle-session reuse threshold (builder use). Floored at 1s.
    pub(crate) fn set_max_idle(&mut self, max_idle: Duration) {
        self.max_idle = max_idle.max(Duration::from_secs(1));
    }

    /// Enable the idle-session keepalive on this interval (builder use).
    /// Floored at [`MIN_KEEPALIVE`].
    pub(crate) fn set_keepalive(&mut self, interval: Duration) {
        self.keepalive = Some(interval.max(MIN_KEEPALIVE));
    }

    /// Spawn the keepalive task once, if configured. Called from the async
    /// acquire paths so a Tokio runtime is guaranteed. Each tick drains every
    /// idle session — Limited pool and UID-Mode pool alike — NOOPs each
    /// OUTSIDE the pool lock (an acquiring caller must never wait behind a
    /// slow dead-socket ping), and returns survivors with refreshed idle
    /// stamps, so they never cross the `max_idle` threshold and the server
    /// never sees them as idle. This is what makes the process behave like a
    /// mainstream mail client: a few long-lived connections, each LOGINed
    /// once, instead of a login per gap in traffic. A failed ping drops that
    /// session; the next acquire reconnects fresh.
    fn ensure_keepalive(&self) {
        let Some(interval) = self.keepalive else {
            return;
        };
        let mut slot = self.keepalive_task.lock();
        if slot.is_some() {
            return;
        }
        let stores = [Arc::clone(&self.pools), Arc::clone(&self.uid_pools)];
        *slot = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                for store in &stores {
                    let accounts: Vec<String> = store.lock().await.keys().cloned().collect();
                    for account in accounts {
                        // Take the whole idle set while locked; ping outside
                        // the lock; return survivors.
                        let idle_set = store
                            .lock()
                            .await
                            .get_mut(&account)
                            .map(std::mem::take)
                            .unwrap_or_default();
                        let mut alive = Vec::with_capacity(idle_set.len());
                        for mut idle in idle_set {
                            if imap_client::ping(&mut idle.session).await.is_ok() {
                                idle.idle_since = Instant::now();
                                alive.push(idle);
                            }
                            // else: dead — dropped; the next acquire reconnects.
                        }
                        if !alive.is_empty() {
                            store.lock().await.entry(account).or_default().extend(alive);
                        }
                    }
                }
            }
        }));
    }

    /// Whether the keepalive task has been spawned (test observability).
    #[cfg(test)]
    fn keepalive_running(&self) -> bool {
        self.keepalive_task.lock().is_some()
    }

    /// Record that the server rate-limited a LOGIN for this account: arm the
    /// cooldown, or escalate it when this LIMIT is consecutive with the
    /// previous episode.
    pub(crate) fn note_login_rate_limit(&self, account_name: &str) {
        let mut cooldowns = self.login_cooldowns.lock();
        let prev = cooldowns.get(account_name).copied();
        let next = next_cooldown(prev, self.login_cooldown, Instant::now());
        cooldowns.insert(account_name.to_string(), next);
        drop(cooldowns);
        tracing::warn!(
            target: "agentmail",
            account = account_name,
            strikes = next.strikes,
            cooldown_s = cooldown_after_strikes(self.login_cooldown, next.strikes).as_secs(),
            "LOGIN rate limited; escalating cooldown"
        );
    }

    /// A successful fresh LOGIN ends the LIMIT episode: clear the account's
    /// strike history. Fresh connects only — idle reuse proves nothing about
    /// the LOGIN gate.
    pub(crate) fn note_login_success(&self, account_name: &str) {
        self.login_cooldowns.lock().remove(account_name);
    }

    /// Time left before this account may attempt a fresh LOGIN again.
    /// (Production reads go through [`Self::login_cooldown_status`] for the
    /// strike count; this simpler view serves the tests.)
    #[cfg(test)]
    pub(crate) fn login_cooldown_remaining(&self, account_name: &str) -> Option<Duration> {
        self.login_cooldown_status(account_name)
            .map(|(remaining, _)| remaining)
    }

    /// Remaining gate time plus the strike count that produced it, for the
    /// strike-aware fast-fail message. `None` once the gate has lifted.
    pub(crate) fn login_cooldown_status(&self, account_name: &str) -> Option<(Duration, u32)> {
        let state = *self.login_cooldowns.lock().get(account_name)?;
        cooldown_remaining(state.until, Instant::now()).map(|remaining| (remaining, state.strikes))
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

    /// Pop one idle session for the account and return it if it is fresh
    /// enough to reuse AND still answers a NOOP; a stale or dead candidate is
    /// dropped (connection closes) and `None` is returned. The pool lock is
    /// held only for the pop — never across the ping.
    async fn try_reuse_idle(&self, account_name: &str) -> Option<ImapSession> {
        let maybe_idle = {
            let mut pools = self.pools.lock().await;
            pools.get_mut(account_name).and_then(|pool| pool.pop())
        }; // lock released here — before any network I/O
        let idle = maybe_idle?;
        let mut session = idle.session;
        if idle_is_fresh(idle.idle_since.elapsed(), self.max_idle)
            && imap_client::ping(&mut session).await.is_ok()
        {
            return Some(session);
        }
        // too old or stale → `session` drops here (connection closes)
        None
    }

    /// Acquire a session for the named account.
    /// Blocks if the per-account concurrency limit is reached.
    ///
    /// Fresh connects are SINGLEFLIGHTED per account: at most one task runs
    /// `imap_client::connect` (a LOGIN) at a time. Queued waiters re-check
    /// the idle pool and the login-cooldown gate once the winner finishes —
    /// a LIMITed winner arms the gate before releasing the guard, so every
    /// waiter fast-fails without burning another LOGIN.
    pub async fn acquire(&self, account_name: &str) -> crate::Result<PooledSession> {
        self.ensure_keepalive();
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

        // Fast path: reuse an idle session. The age check is what keeps
        // AgentMail working after a long idle: a session idle past `max_idle`
        // is very likely dead, so we skip the ~15s dead-`NOOP` ping and
        // reconnect straight away.
        if let Some(session) = self.try_reuse_idle(account_name).await {
            return Ok(PooledSession {
                session: Some(session),
                account_name: account_name.to_string(),
                pool: Arc::clone(&self.pools),
                uid_pool: Arc::clone(&self.uid_pools),
                uid_mode: false,
                max_connections: max_conn,
                _permit: permit,
            });
        }
        // No reusable session — a fresh connect is needed.

        // Login-rate-limit gate, checked BEFORE queueing on the connect lock:
        // only fresh connections LOGIN, so only they are refused during the
        // cooldown (idle reuse above stays available), and an armed-gate
        // caller fast-fails instead of waiting behind an in-flight connect.
        if let Some((remaining, strikes)) = self.login_cooldown_status(account_name) {
            return Err(cooldown_error(account_name, remaining, strikes));
        }

        // Connect singleflight: serialize fresh LOGINs per account. The
        // tokio guard is deliberately held across the connect await (allowed;
        // parking_lot guards are not) and spans connect's internal transient
        // retries — those are LOGINs too.
        let connect_lock = self.account_connect_lock(account_name);
        let _connect_guard = connect_lock.lock().await;

        // Re-check under the guard: a concurrent release may have pooled a
        // session while we queued...
        if let Some(session) = self.try_reuse_idle(account_name).await {
            return Ok(PooledSession {
                session: Some(session),
                account_name: account_name.to_string(),
                pool: Arc::clone(&self.pools),
                uid_pool: Arc::clone(&self.uid_pools),
                uid_mode: false,
                max_connections: max_conn,
                _permit: permit,
            });
        }
        // ...and a LIMITed winner armed the gate before releasing the guard —
        // this is the check that stops queued waiters from burning LOGINs.
        if let Some((remaining, strikes)) = self.login_cooldown_status(account_name) {
            return Err(cooldown_error(account_name, remaining, strikes));
        }

        // Create new connection (password fetched only after the gate, so
        // fast-failing waiters never touch the keychain).
        let password = crate::credentials::get_password(account_name, account_config).await?;
        let session = match imap_client::connect(account_config, &password).await {
            Ok(session) => {
                // A fresh LOGIN was accepted — the LIMIT episode is over.
                self.note_login_success(account_name);
                session
            }
            Err(error) => {
                if imap_client::is_login_rate_limit(&error) {
                    self.note_login_rate_limit(account_name);
                }
                return Err(error);
            }
        };

        Ok(PooledSession {
            session: Some(session),
            account_name: account_name.to_string(),
            pool: Arc::clone(&self.pools),
            uid_pool: Arc::clone(&self.uid_pools),
            uid_mode: false,
            max_connections: max_conn,
            _permit: permit,
        })
    }

    /// Try to reuse an idle UID-Mode session for this account without paying a
    /// LOGIN + `ENABLE UIDONLY`. Returns `None` when no live one is pooled —
    /// the caller then acquires normally and enables UID Mode itself.
    pub async fn try_acquire_uid_mode(
        &self,
        account_name: &str,
    ) -> crate::Result<Option<PooledSession>> {
        self.ensure_keepalive();
        if !self.config.accounts.contains_key(account_name) {
            return Err(AgentmailError::AccountNotFound(account_name.to_string()));
        }
        let max_conn = self.account_max_connections(account_name);
        let sem = self.account_semaphore(account_name).await;
        let permit = sem
            .acquire_owned()
            .await
            .map_err(|_| AgentmailError::Other("concurrency semaphore closed".to_string()))?;

        let maybe_idle = {
            let mut pools = self.uid_pools.lock().await;
            pools.get_mut(account_name).and_then(|pool| pool.pop())
        };
        if let Some(idle) = maybe_idle {
            let mut session = idle.session;
            if idle_is_fresh(idle.idle_since.elapsed(), self.max_idle)
                && imap_client::ping(&mut session).await.is_ok()
            {
                return Ok(Some(PooledSession {
                    session: Some(session),
                    account_name: account_name.to_string(),
                    pool: Arc::clone(&self.pools),
                    uid_pool: Arc::clone(&self.uid_pools),
                    uid_mode: true,
                    max_connections: max_conn,
                    _permit: permit,
                }));
            }
            // too old or stale → drops here (connection closes)
        }
        drop(permit); // the caller's normal acquire takes its own permit
        Ok(None)
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
    /// The UID-Mode idle store; `release` routes here when `uid_mode` is set.
    uid_pool: Arc<Mutex<HashMap<String, Vec<IdleSession>>>>,
    /// Set once `ENABLE UIDONLY` succeeded on this connection. Sticky for the
    /// connection's life, so release must never return it to the Limited pool.
    uid_mode: bool,
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

    /// Mark this connection as UID-Mode (after a successful `ENABLE UIDONLY`).
    /// From here on `release` returns it to the UID-Mode store, never the
    /// Limited pool.
    pub fn mark_uid_mode(&mut self) {
        self.uid_mode = true;
    }

    /// Return the session to the pool for reuse — the UID-Mode store when this
    /// connection has UIDONLY enabled, the Limited pool otherwise.
    /// The concurrency permit is released when this PooledSession is dropped.
    pub async fn release(mut self) {
        if let Some(session) = self.session.take() {
            let (store, cap) = if self.uid_mode {
                (&self.uid_pool, MAX_IDLE_UID_MODE)
            } else {
                (&self.pool, self.max_connections)
            };
            let mut pools = store.lock().await;
            let pool = pools.entry(self.account_name.clone()).or_default();
            if pool.len() < cap {
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

impl Drop for ConnectionPool {
    fn drop(&mut self) {
        // Stop the keepalive ticker with its pool; pooled sessions close as
        // their Arcs unwind.
        if let Some(task) = self.keepalive_task.lock().take() {
            task.abort();
        }
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
            idle_is_fresh(Duration::from_secs(0), MAX_IDLE),
            "fresh session reusable"
        );
        assert!(
            idle_is_fresh(MAX_IDLE - Duration::from_secs(1), MAX_IDLE),
            "just under the threshold is still reusable",
        );
        assert!(
            !idle_is_fresh(MAX_IDLE, MAX_IDLE),
            "exactly at the threshold is evicted"
        );
        assert!(
            !idle_is_fresh(MAX_IDLE + Duration::from_secs(30 * 60), MAX_IDLE),
            "long-idle (likely server-dropped) session is evicted",
        );
        // A raised threshold (builder .max_idle) keeps older sessions eligible.
        assert!(
            idle_is_fresh(
                MAX_IDLE + Duration::from_secs(60),
                Duration::from_secs(20 * 60)
            ),
            "a raised max_idle admits sessions the default would evict",
        );
    }

    /// The keepalive task spawns once on first pool use when configured, and
    /// never spawns when it is not.
    #[tokio::test]
    async fn keepalive_spawns_once_and_only_when_configured() {
        let unconfigured = ConnectionPool::new(Config::empty());
        unconfigured.ensure_keepalive();
        assert!(
            !unconfigured.keepalive_running(),
            "no keepalive without opt-in"
        );

        let mut pool = ConnectionPool::new(Config::empty());
        pool.set_keepalive(Duration::from_secs(1));
        assert!(
            pool.keepalive == Some(MIN_KEEPALIVE),
            "sub-floor intervals clamp to MIN_KEEPALIVE"
        );
        assert!(!pool.keepalive_running(), "lazy: nothing spawned at build");
        pool.ensure_keepalive();
        assert!(pool.keepalive_running(), "first use spawns the ticker");
        pool.ensure_keepalive();
        assert!(
            pool.keepalive_running(),
            "second ensure is a no-op, not a respawn"
        );
        drop(pool); // Drop aborts the task; nothing to assert beyond no panic.
    }

    #[test]
    fn cooldown_remaining_counts_down_and_lifts() {
        let now = Instant::now();
        let until = now + Duration::from_secs(120);
        assert_eq!(
            cooldown_remaining(until, now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            cooldown_remaining(until, now + Duration::from_secs(60)),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            cooldown_remaining(until, until),
            None,
            "the gate lifts exactly at the deadline"
        );
        assert_eq!(
            cooldown_remaining(until, until + Duration::from_secs(1)),
            None
        );
    }

    /// The login-rate-limit gate: noting a LIMIT arms a cooldown for that
    /// account only, and it reports time remaining until it lifts.
    #[test]
    fn login_rate_limit_gate_is_per_account() {
        let pool = ConnectionPool::new(Config::empty());
        assert!(pool.login_cooldown_remaining("aol").is_none());

        pool.note_login_rate_limit("aol");
        let remaining = pool
            .login_cooldown_remaining("aol")
            .expect("cooldown armed for the limited account");
        assert!(remaining <= LOGIN_RATE_LIMIT_COOLDOWN);
        assert!(remaining > LOGIN_RATE_LIMIT_COOLDOWN - Duration::from_secs(5));
        assert!(
            pool.login_cooldown_remaining("gmail").is_none(),
            "other accounts keep connecting"
        );
    }

    #[test]
    fn cooldown_after_strikes_doubles_and_caps() {
        let base = Duration::from_secs(300);
        assert_eq!(cooldown_after_strikes(base, 1), Duration::from_secs(300));
        assert_eq!(cooldown_after_strikes(base, 2), Duration::from_secs(600));
        assert_eq!(cooldown_after_strikes(base, 3), Duration::from_secs(1200));
        assert_eq!(cooldown_after_strikes(base, 4), Duration::from_secs(2400));
        assert_eq!(cooldown_after_strikes(base, 5), Duration::from_secs(3600));
        assert_eq!(
            cooldown_after_strikes(base, 6),
            Duration::from_secs(3600),
            "cap holds"
        );
        assert_eq!(
            cooldown_after_strikes(base, 30),
            Duration::from_secs(3600),
            "large strike counts stay capped (overflow-safe)"
        );
        // A tiny base walks the ladder further before the cap.
        assert_eq!(
            cooldown_after_strikes(Duration::from_secs(1), 12),
            Duration::from_secs(2048)
        );
        assert_eq!(
            cooldown_after_strikes(Duration::from_secs(1), 13),
            Duration::from_secs(3600)
        );
        // An over-cap builder base is honored, not shrunk.
        let big = Duration::from_secs(7200);
        assert_eq!(cooldown_after_strikes(big, 1), big);
        assert_eq!(cooldown_after_strikes(big, 2), big);
        // Defensive cases: strike 0 behaves as 1; u32::MAX does not panic.
        assert_eq!(cooldown_after_strikes(base, 0), base);
        assert_eq!(
            cooldown_after_strikes(base, u32::MAX),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn next_cooldown_first_limit_is_strike_one() {
        let base = Duration::from_secs(300);
        let now = Instant::now();
        let state = next_cooldown(None, base, now);
        assert_eq!(state.strikes, 1);
        assert_eq!(state.until, now + base);
    }

    #[test]
    fn next_cooldown_relimit_within_window_escalates() {
        let base = Duration::from_secs(300);
        let t0 = Instant::now();
        let first = next_cooldown(None, base, t0);
        // Re-LIMIT while still armed → strike 2.
        let while_armed = next_cooldown(Some(first), base, t0 + Duration::from_secs(100));
        assert_eq!(while_armed.strikes, 2);
        // Re-LIMIT shortly AFTER expiry but inside the 2× grace window — the
        // "server penalty outlasts our cooldown" probe case — also escalates.
        let after_expiry = first.until + Duration::from_secs(599);
        let probed = next_cooldown(Some(first), base, after_expiry);
        assert_eq!(probed.strikes, 2);
        assert_eq!(probed.until, after_expiry + Duration::from_secs(600));
    }

    #[test]
    fn next_cooldown_relimit_at_or_after_window_starts_over() {
        let base = Duration::from_secs(300);
        let t0 = Instant::now();
        let third = LoginCooldown {
            until: t0 + Duration::from_secs(1200),
            strikes: 3,
        };
        // Applied cooldown at strike 3 is 1200s → grace boundary is
        // until + 2400s. At the boundary the episode has lapsed.
        let boundary = third.until + Duration::from_secs(2400);
        assert_eq!(next_cooldown(Some(third), base, boundary).strikes, 1);
        assert_eq!(
            next_cooldown(Some(third), base, boundary + Duration::from_secs(3600)).strikes,
            1
        );
    }

    #[test]
    fn next_cooldown_walks_the_escalation_ladder() {
        let base = Duration::from_secs(300);
        let mut now = Instant::now();
        let mut state: Option<LoginCooldown> = None;
        let mut applied = Vec::new();
        for _ in 0..6 {
            let next = next_cooldown(state, base, now);
            applied.push((next.until - now).as_secs());
            // The next probe fires just after this cooldown expires — the
            // exact leak pattern observed live.
            now = next.until + Duration::from_secs(1);
            state = Some(next);
        }
        assert_eq!(applied, vec![300, 600, 1200, 2400, 3600, 3600]);
    }

    /// Pool-level escalation: consecutive LIMITs raise the strike count and
    /// the window; a successful fresh login resets the episode.
    #[test]
    fn login_rate_limit_gate_escalates_and_success_resets() {
        let pool = ConnectionPool::new(Config::empty());
        pool.note_login_rate_limit("aol");
        pool.note_login_rate_limit("aol");
        let (remaining, strikes) = pool
            .login_cooldown_status("aol")
            .expect("gate armed after consecutive LIMITs");
        assert_eq!(strikes, 2);
        assert!(
            remaining > LOGIN_RATE_LIMIT_COOLDOWN,
            "strike 2 outlasts the base cooldown: {remaining:?}"
        );
        assert!(
            pool.login_cooldown_status("gmail").is_none(),
            "escalation is per-account"
        );

        pool.note_login_success("aol");
        assert!(
            pool.login_cooldown_status("aol").is_none(),
            "a successful fresh login ends the episode"
        );
        pool.note_login_rate_limit("aol");
        let (remaining, strikes) = pool.login_cooldown_status("aol").expect("re-armed");
        assert_eq!(strikes, 1, "post-success LIMIT starts a new episode");
        assert!(remaining <= LOGIN_RATE_LIMIT_COOLDOWN);
    }

    #[test]
    fn cooldown_error_mentions_remaining_and_strike() {
        let error = cooldown_error("aol", Duration::from_secs(600), 2);
        let text = error.to_string();
        assert!(text.contains("rate-limited LOGIN"), "{text}");
        assert!(text.contains("600s"), "{text}");
        assert!(text.contains("strike 2"), "{text}");
        assert!(
            !error.is_connection_error(),
            "the gate error must not trigger connection-retry loops"
        );
    }

    /// The singleflight primitive: one lock per account, shared across
    /// lookups, exclusive while held.
    #[tokio::test]
    async fn connect_lock_is_per_account_and_exclusive() {
        let pool = ConnectionPool::new(Config::empty());
        let a1 = pool.account_connect_lock("a");
        let a2 = pool.account_connect_lock("a");
        let b = pool.account_connect_lock("b");
        assert!(Arc::ptr_eq(&a1, &a2), "same account shares one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "accounts are independent");

        let guard = a1.lock().await;
        assert!(
            a2.try_lock().is_err(),
            "the connect lock is exclusive while a connect is in flight"
        );
        assert!(b.try_lock().is_ok(), "other accounts are not serialized");
        drop(guard);
        assert!(a2.try_lock().is_ok(), "released after the winner finishes");
    }

    /// An armed gate fast-fails acquire before any network or keychain I/O:
    /// the inline account points at a closed local port, so any regression to
    /// actual connecting would surface as a distinctly different error.
    #[tokio::test]
    async fn armed_gate_fast_fails_acquire_before_any_network_io() {
        let config = Config::from_accounts(vec![(
            "aol".to_string(),
            crate::config::AccountConfig {
                host: "127.0.0.1".to_string(),
                port: 1, // closed port — a real connect attempt would error differently
                username: "user@example.com".to_string(),
                password: Some(crate::secret::Secret::new_raw("unused")),
                tls: true,
                max_connections: None,
                auth: crate::config::AuthMethod::Password,
            },
        )]);
        let pool = ConnectionPool::new(config);
        pool.note_login_rate_limit("aol");
        let Err(error) = pool.acquire("aol").await else {
            panic!("armed gate must refuse fresh connects");
        };
        let text = error.to_string();
        assert!(
            text.contains("rate-limited LOGIN") && text.contains("strike 1"),
            "fast-fail carries the strike-aware cooldown message: {text}"
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
