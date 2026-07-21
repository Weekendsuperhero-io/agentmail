use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_imap::Session;
use async_imap::extensions::compress::DeflateStream;
use futures::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use tracing::{debug, warn};

use crate::AgentmailError;
use crate::config::AccountConfig;
use crate::error::Result;
use crate::parser;
use crate::types::*;

/// The concrete IMAP session type used throughout: a TLS stream with RFC 4978
/// DEFLATE compression negotiated on every connection. `DeflateStream` is
/// `!Unpin`, so it is wrapped in `Pin<Box<_>>` (which is `Unpin`) to satisfy
/// async-imap's `Session<T>` bounds. A concrete type — not `dyn` — so it does
/// not reintroduce the higher-ranked-lifetime conflict with
/// `with_session_retry`'s op closures. Every supported provider advertises
/// COMPRESS=DEFLATE; a server lacking it fails the compression handshake at
/// connect.
pub type ImapSession = Session<Pin<Box<DeflateStream<TlsStream<TcpStream>>>>>;

/// Callback for reporting progress: `(completed, total)`.
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Callback for cooperative cancellation: returns `true` once the caller
/// should abandon the operation. Kept as a plain `Fn` so the core library
/// stays free of tokio_util / MCP types.
pub type CancelFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// Bail out with `AgentmailError::Cancelled` when cancelled.
pub(crate) fn check_cancel(cancel: Option<&CancelFn>) -> Result<()> {
    if cancel.is_some_and(|c| c()) {
        return Err(AgentmailError::Cancelled);
    }
    Ok(())
}

/// Type alias for raw fetch items: `(uid, size, flags, body_bytes)`.
type RawFetchItems = Vec<(u32, Option<u32>, Vec<String>, Vec<u8>)>;

/// Default timeout for IMAP operations (connect, login, fetch, etc.).
const IMAP_TIMEOUT: Duration = Duration::from_secs(90);

/// Shorter timeout for keep-alive pings.
const PING_TIMEOUT: Duration = Duration::from_secs(15);

/// Max UIDs addressed in one FETCH command. Aligned with the common RFC 9738
/// `MESSAGELIMIT=1000` so a single command never exceeds a MESSAGELIMIT server.
pub(crate) const MAX_FETCH_CHUNK: usize = 1000;

/// Maximum complete source accepted for one-click DKIM verification. Ranking
/// never fetches complete messages; this bounds the one action-time exception
/// without imposing any ceiling on matching-message cleanup.
const MAX_UNSUBSCRIBE_MESSAGE_BYTES: u32 = 64 * 1024 * 1024;
pub(crate) const MAX_TRANSIENT_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Timeout helpers
// ---------------------------------------------------------------------------

/// Wrap a future with the standard IMAP timeout.
///
/// Errors convert via `Into<AgentmailError>` so typed variants survive —
/// the pool's connection-error retry and the MCP error mapping both depend
/// on seeing `AgentmailError::Imap(...)` rather than a stringified `Other`.
async fn imap_timeout<F, T, E>(future: F) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: Into<AgentmailError>,
{
    match tokio::time::timeout(IMAP_TIMEOUT, future).await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(e)) => Err(e.into()),
        Err(_elapsed) => Err(AgentmailError::Other(format!(
            "IMAP operation timed out after {}s",
            IMAP_TIMEOUT.as_secs()
        ))),
    }
}

/// UID FETCH + stream collect with timeout.
async fn timed_uid_fetch_collect<T>(
    session: &mut Session<T>,
    uid_set: &str,
    items: &str,
) -> Result<Vec<std::result::Result<async_imap::types::Fetch, async_imap::error::Error>>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    imap_timeout(async {
        let stream = session.uid_fetch(uid_set, items).await?;
        Ok::<_, async_imap::error::Error>(stream.collect::<Vec<_>>().await)
    })
    .await
}

/// Select a mailbox with timeout. Use this instead of calling `session.select()` directly.
/// Yahoo/AOL intermittently answer SELECT/EXAMINE with
/// `NO [SERVERBUG] ... Please try again later`. It is genuinely transient;
/// honor the server's own advice with short backoff retries before
/// surfacing the failure.
const MAILBOX_OPEN_ATTEMPTS: usize = 3;

fn is_transient_server_bug(error: &AgentmailError) -> bool {
    matches!(
        error,
        AgentmailError::Imap(async_imap::error::Error::No(text))
            if text.to_uppercase().contains("SERVERBUG")
    )
}

pub async fn select<T>(
    session: &mut Session<T>,
    mailbox: &str,
) -> Result<async_imap::types::Mailbox>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    for attempt in 1..=MAILBOX_OPEN_ATTEMPTS {
        match imap_timeout(session.select(mailbox)).await {
            Err(error) if is_transient_server_bug(&error) && attempt < MAILBOX_OPEN_ATTEMPTS => {
                debug!(
                    target: "agentmail",
                    mailbox,
                    attempt,
                    "transient SERVERBUG opening mailbox; retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(1_500 * attempt as u64)).await;
            }
            other => return other,
        }
    }
    unreachable!("the final attempt always returns")
}

/// Examine a mailbox read-only with timeout. Discovery scans use this instead
/// of `SELECT` so opening a mailbox cannot consume or alter `\Recent` state.
pub async fn examine<T>(
    session: &mut Session<T>,
    mailbox: &str,
) -> Result<async_imap::types::Mailbox>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    for attempt in 1..=MAILBOX_OPEN_ATTEMPTS {
        match imap_timeout(session.examine(mailbox)).await {
            Err(error) if is_transient_server_bug(&error) && attempt < MAILBOX_OPEN_ATTEMPTS => {
                debug!(
                    target: "agentmail",
                    mailbox,
                    attempt,
                    "transient SERVERBUG examining mailbox; retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(1_500 * attempt as u64)).await;
            }
            other => return other,
        }
    }
    unreachable!("the final attempt always returns")
}

/// Require a usable UIDVALIDITY value from a successful SELECT or EXAMINE.
///
/// RFC 3501 and RFC 9051 require servers to advertise a non-zero UIDVALIDITY
/// when a mailbox is opened. Treat a missing or zero value as unavailable so
/// no mailbox-local UID can escape without its epoch.
pub(crate) fn require_uid_validity(mailbox: &str, actual: Option<u32>) -> Result<u32> {
    match actual {
        Some(actual) if actual != 0 => Ok(actual),
        _ => Err(AgentmailError::UidValidityUnavailable {
            mailbox: mailbox.to_string(),
        }),
    }
}

/// Validate a caller-provided UID epoch against the mailbox just opened.
///
/// Callers should reject zero before acquiring a connection. This second
/// check is deliberately retained at the IMAP boundary so an internal caller
/// cannot accidentally execute a UID command with an incomplete identity.
pub(crate) fn validate_expected_uid_validity(
    mailbox: &str,
    expected: u32,
    actual: Option<u32>,
) -> Result<u32> {
    if expected == 0 {
        return Err(AgentmailError::UidValidityChanged {
            mailbox: mailbox.to_string(),
            expected,
            actual,
        });
    }

    let actual = require_uid_validity(mailbox, actual)?;
    if actual != expected {
        return Err(AgentmailError::UidValidityChanged {
            mailbox: mailbox.to_string(),
            expected,
            actual: Some(actual),
        });
    }
    Ok(actual)
}

/// SELECT a mailbox and validate its UID epoch before any UID command or
/// side effect is attempted.
pub(crate) async fn select_with_expected_uid_validity<T>(
    session: &mut Session<T>,
    mailbox: &str,
    expected_uid_validity: u32,
) -> Result<async_imap::types::Mailbox>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let selected = imap_timeout(session.select(mailbox)).await?;
    validate_expected_uid_validity(mailbox, expected_uid_validity, selected.uid_validity)?;
    Ok(selected)
}

/// EXAMINE a mailbox and validate its UID epoch before a read-only UID
/// command is attempted.
pub(crate) async fn examine_with_expected_uid_validity<T>(
    session: &mut Session<T>,
    mailbox: &str,
    expected_uid_validity: u32,
) -> Result<async_imap::types::Mailbox>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let selected = examine(session, mailbox).await?;
    validate_expected_uid_validity(mailbox, expected_uid_validity, selected.uid_validity)?;
    Ok(selected)
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Maximum connect attempts (1 initial + retries) for a single `connect`.
/// Kept small so a genuinely wrong password surfaces quickly and cannot
/// hammer the server into a rate-limit / lockout.
const MAX_CONNECT_ATTEMPTS: usize = 3;

/// Whether a failed connect is worth retrying with backoff. Covers transient
/// network drops AND transient auth rejections: iCloud/Gmail routinely reply
/// `[AUTHENTICATIONFAILED]` to a login (especially several in quick
/// succession) and then accept the same credentials moments later. We can't
/// distinguish a transient rejection from a truly-wrong password, so the
/// retry count is deliberately tiny.
fn is_retryable_connect_error(e: &AgentmailError) -> bool {
    e.is_connection_error() || matches!(e, AgentmailError::Imap(async_imap::error::Error::No(_)))
}

/// Yahoo/AOL reject over-eager logins with `NO [LIMIT] LOGIN Rate limit
/// hit.` — sub-second retries only extend the penalty window, so these get
/// materially longer spacing than an ordinary transient failure.
fn is_login_rate_limit(e: &AgentmailError) -> bool {
    matches!(
        e,
        AgentmailError::Imap(async_imap::error::Error::No(text))
            if text.to_uppercase().contains("RATE LIMIT")
    )
}

/// Send the RFC 5161 `ENABLE` command for one or more extensions and return
/// the capabilities the server confirms in its `* ENABLED …` response.
///
/// async-imap exposes no `enable` method, so this is written directly on the
/// `run_command` / `read_response` primitives. It is the first building block
/// toward Yahoo/AOL UID Mode (`ENABLE UIDONLY`), which lifts the visible-window
/// limit — though UID Mode is not usable until the parser handles `UIDFETCH`
/// responses (imap-proto gap; see docs/standards/imap/yahoo-aol-quirks.md).
/// Safe to call for CONDSTORE/QRESYNC/UTF8=ACCEPT, which do not change the
/// response framing.
pub async fn enable<T>(session: &mut Session<T>, extensions: &str) -> Result<Vec<String>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    use async_imap::imap_proto::{Response, Status};

    let tag = imap_timeout(session.run_command(format!("ENABLE {extensions}"))).await?;
    let mut enabled: Vec<String> = Vec::new();
    loop {
        let response = imap_timeout(session.read_response()).await?;
        let Some(response) = response else {
            return Err(AgentmailError::NotConnected);
        };
        match response.parsed() {
            // `* ENABLED <caps>` — imap-proto surfaces it as Capabilities.
            Response::Capabilities(capabilities) => {
                enabled.extend(
                    capabilities
                        .iter()
                        .map(|capability| format!("{capability:?}")),
                );
            }
            Response::Done {
                tag: done,
                status,
                information,
                ..
            } if done == &tag => {
                return match status {
                    Status::Ok => Ok(enabled),
                    other => Err(AgentmailError::Other(format!(
                        "ENABLE {extensions} rejected: {other:?} {information:?}"
                    ))),
                };
            }
            // Unilateral/unsolicited responses may interleave; skip them.
            _ => {}
        }
    }
}

/// Enumerate EVERY UID in the currently selected mailbox in RFC 9586 UID Mode,
/// bypassing the Limited-Mode visible window. The caller must have enabled
/// UIDONLY ([`enable`]) and selected the mailbox.
///
/// UID Mode caps each `SEARCH`/`FETCH` at `MESSAGELIMIT` results, so this walks
/// the mailbox newest-to-oldest in `MESSAGELIMIT`-sized `PARTIAL` windows
/// (`UID FETCH 1:* (UID) (PARTIAL -lo:-hi)`) until a short page marks the
/// oldest message. Returns ascending, deduplicated UIDs — the full mailbox,
/// not just the window.
pub async fn walk_all_uids_uidmode<T>(
    session: &mut Session<T>,
    message_limit: u32,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let window = message_limit.clamp(1, MAX_FETCH_CHUNK as u32);
    // Fixed "newest `window`" partial; pagination shrinks the UID RANGE, not
    // the partial offset. This is the idiom in Yahoo/AOL's own docs — an
    // incrementing partial like `-1001:-2000` returns nothing there.
    let items = format!("(UID) (PARTIAL -1:-{window})");
    let mut all: Vec<u32> = Vec::new();
    let mut upper: Option<u32> = None; // None → "1:*" for the first page
    loop {
        check_cancel(cancel)?;
        let range = match upper {
            None => "1:*".to_string(),
            Some(hi) => format!("1:{hi}"),
        };
        let fetched = timed_uid_fetch_collect(session, &range, &items).await?;
        let mut page = 0u32;
        let mut lowest: Option<u32> = None;
        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            if let Some(uid) = fetch.uid {
                all.push(uid);
                lowest = Some(lowest.map_or(uid, |current| current.min(uid)));
                page += 1;
            }
        }
        if let Some(progress) = on_progress {
            progress(all.len() as u64, all.len() as u64);
        }
        // A short page means we reached the oldest message.
        if page < window {
            break;
        }
        // Next page covers UIDs strictly below the lowest one just seen.
        match lowest {
            Some(low) if low > 1 => upper = Some(low - 1),
            _ => break,
        }
    }
    all.sort_unstable();
    all.dedup();
    Ok(all)
}

/// Connect to an IMAP server over TLS and authenticate, retrying a transient
/// failure a few times with backoff (fresh connection each attempt).
pub async fn connect(config: &AccountConfig, password: &str) -> Result<ImapSession> {
    let mut backoff = Duration::from_millis(400);
    for attempt in 1..=MAX_CONNECT_ATTEMPTS {
        match connect_once(config, password).await {
            Ok(session) => return Ok(session),
            Err(e) if attempt < MAX_CONNECT_ATTEMPTS && is_retryable_connect_error(&e) => {
                let wait = if is_login_rate_limit(&e) {
                    // Respect the rate limiter: sub-second retries only
                    // extend the penalty window.
                    Duration::from_secs(5 * attempt as u64)
                } else {
                    backoff
                };
                debug!(
                    target: "agentmail",
                    attempt,
                    max = MAX_CONNECT_ATTEMPTS,
                    wait_ms = wait.as_millis() as u64,
                    "transient connect/login failure; retrying after backoff"
                );
                tokio::time::sleep(wait).await;
                backoff *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// A single TLS connect + login, no retry.
async fn connect_once(config: &AccountConfig, password: &str) -> Result<ImapSession> {
    let addr = format!("{}:{}", config.host, config.port);
    let tcp = imap_timeout(TcpStream::connect(&addr)).await?;

    let connector = native_tls::TlsConnector::new()
        .map_err(|e| AgentmailError::Other(format!("TLS connector error: {}", e)))?;
    let connector = tokio_native_tls::TlsConnector::from(connector);
    let tls = imap_timeout(connector.connect(&config.host, tcp)).await?;

    let client = async_imap::Client::new(tls);
    let login_fut = client.login(&config.username, password);
    let plain = match tokio::time::timeout(IMAP_TIMEOUT, login_fut).await {
        Ok(Ok(session)) => session,
        Ok(Err((err, _client))) => return Err(AgentmailError::Imap(err)),
        Err(_elapsed) => {
            return Err(AgentmailError::Other(format!(
                "IMAP login timed out after {}s",
                IMAP_TIMEOUT.as_secs()
            )));
        }
    };

    // Negotiate DEFLATE compression. `compress` runs `COMPRESS DEFLATE` and
    // wraps the stream; `Box::pin` gives the `Unpin` type the alias requires.
    let mut session = imap_timeout(plain.compress(Box::pin)).await?;
    send_client_id(&mut session).await;
    Ok(session)
}

/// Identify ourselves via the RFC 2971 `ID` command right after login.
///
/// Yahoo/AOL treat unidentified clients less favorably (documented rate
/// limiting and `[SERVERBUG]` responses; this is why Delta Chat — async-imap's
/// primary consumer — sends `ID` to Yahoo). Best-effort: `ID` is advisory, so
/// a server that rejects or lacks it must not fail the connection. Runs under
/// the ping timeout so a hung `ID` cannot stall a connect.
async fn send_client_id(session: &mut ImapSession) {
    // Yahoo/AOL's own IMAP docs ask clients to send name, version, and os.
    let identification = [
        ("name", Some("AgentMail")),
        ("version", Some(env!("CARGO_PKG_VERSION"))),
        ("os", Some(std::env::consts::OS)),
    ];
    match tokio::time::timeout(PING_TIMEOUT, session.id(identification)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => debug!(
            target: "agentmail",
            error = %error,
            "server rejected the ID command; continuing without identification"
        ),
        Err(_elapsed) => debug!(
            target: "agentmail",
            "ID command timed out; continuing without identification"
        ),
    }
}

/// Validate a session is still alive with NOOP.
pub async fn ping(session: &mut ImapSession) -> Result<()> {
    match tokio::time::timeout(PING_TIMEOUT, session.noop()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(AgentmailError::Imap(e)),
        Err(_) => Err(AgentmailError::Other("IMAP ping timed out".into())),
    }
}

/// Query server capabilities via IMAP CAPABILITY command.
pub async fn list_capabilities(session: &mut ImapSession) -> Result<Vec<String>> {
    let mut result = capability_strings(session).await?;
    result.sort();
    Ok(result)
}

/// Collect raw capability tokens (uppercased on the wire varies by server, so
/// callers normalize). `AUTH=` mechanisms are flattened to `AUTH=<mech>`.
async fn capability_strings(session: &mut ImapSession) -> Result<Vec<String>> {
    let caps = imap_timeout(session.capabilities()).await?;
    Ok(caps
        .iter()
        .map(|c| match c {
            async_imap::types::Capability::Imap4rev1 => "IMAP4rev1".to_string(),
            async_imap::types::Capability::Auth(s) => format!("AUTH={}", s),
            async_imap::types::Capability::Atom(s) => s.clone(),
        })
        .collect())
}

/// Parsed server capabilities, used to gate command variants (MOVE, UIDPLUS,
/// IMAP4rev1-vs-rev2). Tokens are stored uppercased for case-insensitive lookup.
#[derive(Debug, Clone, Default)]
pub struct ServerCaps {
    tokens: hashbrown::HashSet<String>,
}

impl ServerCaps {
    pub fn from_strings<I: IntoIterator<Item = String>>(caps: I) -> Self {
        Self {
            tokens: caps.into_iter().map(|c| c.to_uppercase()).collect(),
        }
    }

    /// Fetch and parse the server's capabilities.
    pub async fn fetch(session: &mut ImapSession) -> Result<Self> {
        Ok(Self::from_strings(capability_strings(session).await?))
    }

    pub fn has(&self, cap: &str) -> bool {
        self.tokens.contains(&cap.to_uppercase())
    }

    /// RFC 6851 MOVE / UID MOVE.
    pub fn has_move(&self) -> bool {
        self.has("MOVE")
    }

    /// RFC 4315 UIDPLUS (enables UID EXPUNGE for targeted, concurrent-safe deletes).
    pub fn has_uidplus(&self) -> bool {
        self.has("UIDPLUS")
    }

    /// Whether the server speaks IMAP4rev1 (RFC 3501). IMAP4rev2-only servers
    /// (RFC 9051) removed the RECENT data item from STATUS.
    pub fn has_imap4rev1(&self) -> bool {
        self.has("IMAP4REV1")
    }

    /// Whether this is Gmail (advertises the `X-GM-EXT-1` extension). On Gmail,
    /// `\Deleted` + EXPUNGE in a label folder only removes that label, so
    /// deletes must move the message to `[Gmail]/Trash` instead.
    pub fn is_gmail(&self) -> bool {
        self.has("X-GM-EXT-1")
    }

    /// RFC 7162 CONDSTORE (HIGHESTMODSEQ / CHANGEDSINCE).
    pub fn has_condstore(&self) -> bool {
        self.has("CONDSTORE")
    }

    /// RFC 9738 `MESSAGELIMIT=N`: the server caps how many messages one
    /// command may reference (Yahoo/AOL advertise `MESSAGELIMIT=1000`).
    /// Commands over the limit may be rejected or silently truncated, so
    /// membership discovery must window itself accordingly.
    pub fn message_limit(&self) -> Option<u32> {
        self.tokens.iter().find_map(|token| {
            token
                .strip_prefix("MESSAGELIMIT=")
                .and_then(|value| value.parse().ok())
        })
    }
}

// ---------------------------------------------------------------------------
// Mailbox operations
// ---------------------------------------------------------------------------

/// List selectable mailboxes for an account. Uses LIST + STATUS per result.
/// Preserve every recognized registered IMAP special-use attribute.
fn roles_from_attributes(attrs: &[async_imap::types::NameAttribute<'_>]) -> Vec<String> {
    use async_imap::types::NameAttribute;
    let mut roles = Vec::new();
    for attr in attrs {
        let role = match attr {
            NameAttribute::All => "all",
            NameAttribute::Archive => "archive",
            NameAttribute::Drafts => "drafts",
            NameAttribute::Flagged => "flagged",
            NameAttribute::Junk => "junk",
            NameAttribute::Sent => "sent",
            NameAttribute::Trash => "trash",
            // imap-proto 0.16 predates these registered extensions. Keep the
            // current IANA special-use set available to scan policy anyway.
            NameAttribute::Extension(value) => {
                match value.trim_start_matches('\\').to_ascii_lowercase().as_str() {
                    "important" => "important",
                    "memos" => "memos",
                    "scheduled" => "scheduled",
                    "snoozed" => "snoozed",
                    _ => continue,
                }
            }
            _ => continue,
        };
        if !roles.iter().any(|existing| existing == role) {
            roles.push(role.to_string());
        }
    }
    roles
}

/// RFC 9051 defines `\NonExistent` as an extended LIST attribute with the
/// same selection consequence as `\NoSelect`. imap-proto represents newer
/// attributes through `Extension`, so match it case-insensitively.
fn attributes_are_unselectable(attrs: &[async_imap::types::NameAttribute<'_>]) -> bool {
    use async_imap::types::NameAttribute;

    attrs.iter().any(|attribute| match attribute {
        NameAttribute::NoSelect => true,
        NameAttribute::Extension(value) => value
            .trim_start_matches('\\')
            .eq_ignore_ascii_case("nonexistent"),
        _ => false,
    })
}

pub async fn list_mailboxes(
    session: &mut ImapSession,
    account_name: &str,
    caps: &ServerCaps,
) -> Result<Vec<MailboxInfo>> {
    let (mailboxes, _) = list_mailboxes_page(session, account_name, caps, 0, usize::MAX).await?;
    Ok(mailboxes)
}

/// List one page of selectable mailboxes and issue STATUS only for that page.
///
/// LIST attributes are evaluated before pagination, so `\NoSelect` and
/// rev2 `\NonExistent` containers consume neither page slots nor STATUS
/// round trips. The returned total counts every selectable mailbox.
pub async fn list_mailboxes_page(
    session: &mut ImapSession,
    account_name: &str,
    caps: &ServerCaps,
    offset: usize,
    limit: usize,
) -> Result<(Vec<MailboxInfo>, usize)> {
    let started = std::time::Instant::now();
    let layouts = list_mailbox_layout(session).await?;

    // RFC 9051 (IMAP4rev2) removed the RECENT status item; only request it
    // from servers that still advertise IMAP4rev1, else a rev2-only server
    // replies BAD.
    let status_items = if caps.has_imap4rev1() {
        "(MESSAGES UNSEEN RECENT)"
    } else {
        "(MESSAGES UNSEEN)"
    };

    let (page, total) = selectable_mailbox_page(layouts, offset, limit);
    let mut status_commands = 0usize;
    let mut result = Vec::with_capacity(page.len());
    for layout in page {
        status_commands += 1;
        let status = imap_timeout(session.status(&layout.path, status_items)).await?;
        let (total, unseen, recent) = (status.exists, status.unseen.unwrap_or(0), status.recent);

        let role = layout.primary_role().map(str::to_string);
        result.push(MailboxInfo {
            name: layout.path.clone(),
            account: account_name.to_string(),
            total_messages: total,
            unseen_messages: unseen,
            recent_messages: recent,
            delimiter: layout.delimiter,
            path: layout.path,
            no_select: layout.no_select,
            no_inferiors: layout.no_inferiors,
            role,
            roles: layout.roles,
        });
    }
    debug!(
        target: "agentmail",
        operation = "list_mailboxes",
        elapsed_ms = started.elapsed().as_millis(),
        imap_commands = 1 + status_commands,
        result_count = result.len(),
        "live mailbox listing complete"
    );
    Ok((result, total))
}

fn selectable_mailbox_page(
    layouts: Vec<MailboxLayout>,
    offset: usize,
    limit: usize,
) -> (Vec<MailboxLayout>, usize) {
    let selectable: Vec<_> = layouts
        .into_iter()
        .filter(MailboxLayout::is_selectable)
        .collect();
    let total = selectable.len();
    let page = selectable.into_iter().skip(offset).take(limit).collect();
    (page, total)
}

/// Mailbox hierarchy data that is safe to retain between requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailboxLayout {
    pub(crate) path: String,
    pub(crate) delimiter: Option<String>,
    pub(crate) no_select: bool,
    pub(crate) no_inferiors: bool,
    pub(crate) roles: Vec<String>,
}

impl MailboxLayout {
    pub(crate) fn is_selectable(&self) -> bool {
        !self.no_select
    }

    pub(crate) fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|candidate| candidate == role)
    }

    pub(crate) fn primary_role(&self) -> Option<&str> {
        self.roles.first().map(String::as_str)
    }
}

/// List mailbox hierarchy and attributes without requesting message state.
pub(crate) async fn list_mailbox_layout(session: &mut ImapSession) -> Result<Vec<MailboxLayout>> {
    use async_imap::types::NameAttribute;

    let started = std::time::Instant::now();
    let names: Vec<_> = imap_timeout(async {
        let stream = session.list(Some(""), Some("*")).await?;
        Ok::<_, async_imap::error::Error>(stream.collect::<Vec<_>>().await)
    })
    .await?;

    let mut result = Vec::with_capacity(names.len());
    for item in names {
        let name_ref = item.map_err(AgentmailError::Imap)?;
        let attrs = name_ref.attributes();
        result.push(MailboxLayout {
            path: name_ref.name().to_string(),
            delimiter: name_ref.delimiter().map(str::to_string),
            no_select: attributes_are_unselectable(attrs),
            no_inferiors: attrs.contains(&NameAttribute::NoInferiors),
            roles: roles_from_attributes(attrs),
        });
    }
    debug!(
        target: "agentmail",
        operation = "list_mailbox_layout",
        elapsed_ms = started.elapsed().as_millis(),
        imap_commands = 1,
        result_count = result.len(),
        "live mailbox layout listing complete"
    );
    Ok(result)
}

/// Lightweight mailbox entry: name + key attributes (no STATUS calls).
pub struct MailboxEntry {
    pub name: String,
    pub no_select: bool,
    /// First recognized role, retained for API compatibility.
    pub role: Option<String>,
    /// Every recognized registered special-use role.
    pub roles: Vec<String>,
}

/// List all mailboxes with key attributes (without STATUS calls — much faster
/// than `list_mailboxes`).
pub async fn list_mailbox_entries(session: &mut ImapSession) -> Result<Vec<MailboxEntry>> {
    Ok(list_mailbox_layout(session)
        .await?
        .into_iter()
        .map(|layout| {
            let role = layout.primary_role().map(str::to_string);
            MailboxEntry {
                name: layout.path,
                no_select: layout.no_select,
                role,
                roles: layout.roles,
            }
        })
        .collect())
}

/// List all selectable mailbox names (without STATUS calls).
pub async fn list_mailbox_names(session: &mut ImapSession) -> Result<Vec<String>> {
    let entries = list_mailbox_entries(session).await?;
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.no_select)
        .map(|e| e.name)
        .collect())
}

/// Create a new mailbox on the server.
pub async fn create_mailbox(session: &mut ImapSession, mailbox_name: &str) -> Result<()> {
    imap_timeout(session.create(mailbox_name)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fetch messages
// ---------------------------------------------------------------------------

/// Fetch messages with pagination (newest first by UID descending).
pub async fn fetch_messages(
    session: &mut ImapSession,
    mailbox: &str,
    account_name: &str,
    offset: usize,
    limit: usize,
    include_content: bool,
    include_headers: bool,
) -> Result<(Vec<MessageInfo>, u32, u32)> {
    let started = std::time::Instant::now();
    let mb = imap_timeout(session.select(mailbox)).await?;
    let uid_validity = require_uid_validity(mailbox, mb.uid_validity)?;
    let total = mb.exists;

    if total == 0 {
        debug!(
            target: "agentmail",
            operation = "get_messages",
            elapsed_ms = started.elapsed().as_millis(),
            imap_commands = 1,
            result_count = 0,
            "live message fetch complete"
        );
        return Ok((Vec::new(), 0, uid_validity));
    }

    // Get all UIDs, sort descending (newest first)
    let mut uids = search_all_uids_checked(session, mb.exists, mb.uid_next).await?;
    uids.sort_unstable_by(|a, b| b.cmp(a));

    let start = offset.min(uids.len());
    let end = (start + limit).min(uids.len());
    let page_uids = &uids[start..end];
    debug!(
        offset,
        limit,
        page_count = page_uids.len(),
        "Pagination applied"
    );

    if page_uids.is_empty() {
        debug!(
            target: "agentmail",
            operation = "get_messages",
            elapsed_ms = started.elapsed().as_millis(),
            imap_commands = 2,
            result_count = 0,
            "live message fetch complete"
        );
        return Ok((Vec::new(), total, uid_validity));
    }

    let messages = fetch_by_uids(
        session,
        page_uids,
        mailbox,
        account_name,
        include_content,
        include_headers,
    )
    .await?;
    debug!(
        target: "agentmail",
        operation = "get_messages",
        elapsed_ms = started.elapsed().as_millis(),
        imap_commands = 3,
        result_count = messages.len(),
        "live message fetch complete"
    );
    Ok((messages, total, uid_validity))
}

/// Search messages using IMAP SEARCH, then fetch the matching UIDs.
pub async fn search_messages(
    session: &mut ImapSession,
    mailbox: &str,
    account_name: &str,
    criteria: &SearchCriteria,
    offset: usize,
    limit: usize,
    include_content: bool,
    include_headers: bool,
) -> Result<(Vec<MessageInfo>, u32, u32)> {
    let started = std::time::Instant::now();
    let selected = imap_timeout(session.select(mailbox)).await?;
    let uid_validity = require_uid_validity(mailbox, selected.uid_validity)?;

    let query = build_search_query(criteria)?;
    let mut uids = run_uid_search(session, &query).await?;
    uids.sort_unstable_by(|a, b| b.cmp(a));
    let total_matches = uids.len() as u32;

    let start = offset.min(uids.len());
    let end = (start + limit).min(uids.len());
    let page_uids = &uids[start..end];

    if page_uids.is_empty() {
        debug!(
            target: "agentmail",
            operation = "search_messages",
            elapsed_ms = started.elapsed().as_millis(),
            imap_commands = 2,
            result_count = 0,
            match_count = total_matches,
            "live message search complete"
        );
        return Ok((Vec::new(), total_matches, uid_validity));
    }

    let messages = fetch_by_uids(
        session,
        page_uids,
        mailbox,
        account_name,
        include_content,
        include_headers,
    )
    .await?;
    debug!(
        target: "agentmail",
        operation = "search_messages",
        elapsed_ms = started.elapsed().as_millis(),
        imap_commands = 3,
        result_count = messages.len(),
        match_count = total_matches,
        "live message search complete"
    );
    Ok((messages, total_matches, uid_validity))
}

/// Build an IMAP SEARCH query string from SearchCriteria (public wrapper).
pub fn build_search_query_pub(criteria: &SearchCriteria) -> Result<String> {
    build_search_query(criteria)
}

/// Run a UID SEARCH with a raw query string. Returns matching UIDs.
/// Caller must have already selected the mailbox.
pub async fn search_uids<T>(session: &mut Session<T>, query: &str) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    run_uid_search(session, query).await
}

/// Window size for membership rediscovery when a whole-mailbox
/// `UID SEARCH ALL` comes back short. Matches Yahoo/AOL's advertised
/// RFC 9738 `MESSAGELIMIT=1000` — a `UID lo:hi` span of this width can
/// never reference more messages than the limit.
const SEARCH_WINDOW: u32 = 1_000;

/// Whole-mailbox UID membership with truncation detection.
///
/// async-imap swallows a tagged `NO`/`BAD` on SEARCH (the response stream
/// just ends), so a server that rejects or truncates an over-limit
/// `UID SEARCH ALL` (RFC 9738 MESSAGELIMIT — Yahoo/AOL) yields an empty or
/// short `Ok` rather than an error, which a scan would then publish as a
/// silently incomplete membership. Guard: compare the result count against
/// the `EXISTS` value from the SELECT/EXAMINE the caller just performed; on
/// mismatch, rediscover membership in bounded `UID lo:hi` windows walking up
/// to UIDNEXT. Returns ascending, deduplicated UIDs.
pub async fn search_all_uids_checked<T>(
    session: &mut Session<T>,
    exists: u32,
    uid_next: Option<u32>,
) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let mut uids = run_uid_search(session, "ALL").await?;
    uids.sort_unstable();
    uids.dedup();
    if uids.len() == exists as usize {
        return Ok(uids);
    }
    let Some(uid_next) = uid_next else {
        // Without UIDNEXT there is no window upper bound; a count mismatch
        // may also be a delivery/expunge race, so keep the unwindowed result.
        warn!(
            target: "agentmail",
            expected = exists,
            got = uids.len(),
            "UID SEARCH ALL count mismatch and no UIDNEXT to window by; using result as-is",
        );
        return Ok(uids);
    };

    warn!(
        target: "agentmail",
        expected = exists,
        got = uids.len(),
        "UID SEARCH ALL came back short (server message limit or swallowed NO); rediscovering membership in windows",
    );
    let mut windowed: Vec<u32> = Vec::with_capacity(exists as usize);
    let mut lo: u32 = 1;
    while lo < uid_next {
        let hi = lo
            .saturating_add(SEARCH_WINDOW - 1)
            .min(uid_next.saturating_sub(1));
        windowed.extend(run_uid_search(session, &format!("UID {lo}:{hi}")).await?);
        if hi == u32::MAX {
            break;
        }
        lo = hi + 1;
    }
    windowed.sort_unstable();
    windowed.dedup();
    Ok(windowed)
}

/// Return UID membership metadata for compatibility callers that maintain
/// their own synchronization state.
///
/// When `with_highest_modseq` is true (server advertises CONDSTORE), also
/// requests `HIGHESTMODSEQ`. Falls back without it if the server replies BAD.
pub async fn mailbox_status(
    session: &mut ImapSession,
    mailbox: &str,
    with_highest_modseq: bool,
) -> Result<crate::scan_cache::MailboxStatus> {
    let items = if with_highest_modseq {
        "(UIDVALIDITY UIDNEXT MESSAGES HIGHESTMODSEQ)"
    } else {
        "(UIDVALIDITY UIDNEXT MESSAGES)"
    };
    let status = match imap_timeout(session.status(mailbox, items)).await {
        Ok(s) => s,
        Err(_error) if with_highest_modseq => {
            // Server advertised CONDSTORE but rejected HIGHESTMODSEQ on STATUS
            // (or a proxy stripped it) — fall back to the base triple.
            debug!("STATUS HIGHESTMODSEQ failed; retrying without modseq");
            imap_timeout(session.status(mailbox, "(UIDVALIDITY UIDNEXT MESSAGES)")).await?
        }
        Err(e) => return Err(e),
    };
    Ok(crate::scan_cache::MailboxStatus {
        uid_validity: status.uid_validity,
        uid_next: status.uid_next,
        exists: status.exists,
        highest_modseq: status.highest_modseq,
    })
}

/// Fetch FROM + DATE rows for an explicit UID list (already-selected mailbox).
/// Uses BODY.PEEK to avoid setting `\Seen`. Skips unparseable messages.
pub async fn fetch_sender_dates_for_uids(
    session: &mut ImapSession,
    uids: &[u32],
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<crate::scan_cache::SenderRow>> {
    let total = uids.len() as u64;
    let mut results = Vec::with_capacity(uids.len());
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
        check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetched = timed_uid_fetch_collect(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (FROM DATE Message-ID)])",
        )
        .await?;

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let uid = match fetch.uid {
                Some(u) => u,
                None => continue,
            };
            let header_bytes = fetch.header().unwrap_or(&[]);
            match parser::parse_sender_date(header_bytes) {
                Ok((email, display_name, date, message_id)) => {
                    results.push(crate::scan_cache::SenderRow {
                        uid,
                        email,
                        display_name,
                        date,
                        message_id,
                    });
                }
                Err(_error) => {
                    debug!("fetch_sender_dates: skipping unparseable message");
                }
            }
        }

        completed += chunk.len() as u64;
        if let Some(progress) = on_progress {
            progress(completed, total);
        }
    }

    Ok(results)
}

/// Fetch only FROM and DATE headers for all messages in a mailbox.
/// Selects, searches all UIDs, and fetches them. Uses BODY.PEEK.
pub async fn fetch_sender_dates(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<crate::scan_cache::SenderRow>> {
    let started = std::time::Instant::now();
    let mb = imap_timeout(session.select(mailbox)).await?;
    if mb.exists == 0 {
        trace_live_scan("top_senders", started, 1, 0);
        return Ok(Vec::new());
    }
    let uids = search_all_uids_checked(session, mb.exists, mb.uid_next).await?;
    let fetch_commands = uids.len().div_ceil(1000);
    let results = fetch_sender_dates_for_uids(session, &uids, on_progress, cancel).await?;
    trace_live_scan("top_senders", started, 2 + fetch_commands, results.len());
    Ok(results)
}

/// Fetch the parsed sender (email, display_name) for a single UID.
/// Assumes mailbox is already selected.
pub async fn fetch_sender(session: &mut ImapSession, uid: u32) -> Result<(String, String)> {
    let uid_str = uid.to_string();
    let fetched =
        timed_uid_fetch_collect(session, &uid_str, "BODY.PEEK[HEADER.FIELDS (FROM)]").await?;

    let fetch = fetched
        .into_iter()
        .next()
        .ok_or(AgentmailError::MessageNotFound(uid))?
        .map_err(AgentmailError::Imap)?;

    let header_bytes = fetch.header().unwrap_or(&[]);
    let (email, name, _date, _msgid) = parser::parse_sender_date(header_bytes)?;
    Ok((email, name))
}

/// Fetch the parsed sender (email, display_name) for a batch of UIDs.
/// Returns Vec of (uid, email, display_name). Skips unparseable messages.
pub async fn fetch_senders_batch<T>(
    session: &mut Session<T>,
    uids: &[u32],
    cancel: Option<&CancelFn>,
) -> Result<Vec<(u32, String, String)>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let mut results = Vec::new();
    for chunk in uids.chunks(1000) {
        check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetched =
            timed_uid_fetch_collect(session, &uid_set, "(UID BODY.PEEK[HEADER.FIELDS (FROM)])")
                .await?;

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let uid = match fetch.uid {
                Some(u) => u,
                None => continue,
            };
            let header_bytes = fetch.header().unwrap_or(&[]);
            if let Ok((email, name, _, _)) = parser::parse_sender_date(header_bytes) {
                results.push((uid, email, name));
            }
        }
    }
    Ok(results)
}

/// Parsed immutable headers used by sender and list ranking scans.
///
/// A row can have no List-* fields: the persistent cache stores one marker for
/// every successfully returned UID so non-list messages are not re-fetched.
#[derive(Debug, Clone)]
pub struct ListHeaderRow {
    pub uid: u32,
    /// UIDVALIDITY epoch that gives `uid` its mailbox-local identity.
    /// `None` means the server did not provide UIDVALIDITY, so the row must
    /// not be used as the selector for a later side-effecting operation.
    pub uid_validity: Option<u32>,
    pub list_unsubscribe: Option<String>,
    pub list_unsubscribe_post: Option<String>,
    pub list_id: Option<String>,
    pub sender_email: String,
    pub sender_name: String,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    /// Logical-message identifier, for cross-folder deduplication.
    pub message_id: Option<String>,
}

/// Fetch List-Unsubscribe, List-Unsubscribe-Post, List-Id, FROM, and DATE headers.
/// Only includes messages that have at least one of List-Unsubscribe or
/// List-Unsubscribe-Post, indicating bulk/marketing mail.
pub async fn fetch_list_headers(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<ListHeaderRow>> {
    Ok(fetch_rank_headers(session, mailbox, on_progress, cancel)
        .await?
        .into_iter()
        .filter(has_unsubscribe_header)
        .collect())
}

fn trace_live_scan(
    operation: &'static str,
    started: std::time::Instant,
    imap_commands: usize,
    result_count: usize,
) {
    debug!(
        target: "agentmail",
        operation,
        elapsed_ms = started.elapsed().as_millis(),
        imap_commands,
        result_count,
        "live header scan complete"
    );
}

/// Fetch List-* / FROM / DATE rows for an explicit UID list (already-selected
/// mailbox). Only messages with List-Unsubscribe or List-Unsubscribe-Post are
/// returned (bulk/marketing mail).
pub async fn fetch_list_headers_for_uids(
    session: &mut ImapSession,
    uids: &[u32],
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<ListHeaderRow>> {
    Ok(
        fetch_rank_headers_for_uids(session, uids, on_progress, cancel)
            .await?
            .into_iter()
            .filter(has_unsubscribe_header)
            .collect(),
    )
}

fn has_unsubscribe_header(row: &ListHeaderRow) -> bool {
    row.list_unsubscribe.is_some() || row.list_unsubscribe_post.is_some()
}

/// Fetch the unified immutable ranking-header projection for every message in
/// a mailbox. Uses EXAMINE and BODY.PEEK so discovery has no mailbox side
/// effects.
pub async fn fetch_rank_headers(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<ListHeaderRow>> {
    let started = std::time::Instant::now();
    check_cancel(cancel)?;
    let mb = examine(session, mailbox).await?;
    let uid_validity = require_uid_validity(mailbox, mb.uid_validity)?;
    check_cancel(cancel)?;
    if mb.exists == 0 {
        trace_live_scan("top_rank_headers", started, 1, 0);
        return Ok(Vec::new());
    }
    let mut uids = search_all_uids_checked(session, mb.exists, mb.uid_next).await?;
    check_cancel(cancel)?;
    uids.sort_unstable();
    let fetch_commands = uids.len().div_ceil(1000);
    let mut results = fetch_rank_headers_for_uids(session, &uids, on_progress, cancel).await?;
    if header_fields_look_filtered(&results) {
        warn!(
            target: "agentmail",
            mailbox,
            rows = results.len(),
            "server appears to filter List-Unsubscribe out of HEADER.FIELDS responses; refetching full headers",
        );
        results = fetch_rank_headers_full_for_uids(session, &uids, on_progress, cancel).await?;
    }
    for row in &mut results {
        row.uid_validity = Some(uid_validity);
    }
    trace_live_scan(
        "top_rank_headers",
        started,
        2 + fetch_commands,
        results.len(),
    );
    Ok(results)
}

/// Rank-scan header request: exactly the fields the ranking projection
/// derives from.
const RANK_HEADER_FIELDS_ITEMS: &str = "(UID BODY.PEEK[HEADER.FIELDS (List-Unsubscribe List-Unsubscribe-Post List-Id FROM DATE Message-ID)])";
/// Full-header fallback for servers whose HEADER.FIELDS responses filter out
/// requested fields. Observed on AOL/Yahoo IMAP: `List-Unsubscribe` and
/// `List-Unsubscribe-Post` are omitted while `List-Id`/`From`/`Date`/
/// `Message-ID` from the very same request are returned.
const RANK_HEADER_FULL_ITEMS: &str = "(UID BODY.PEEK[HEADER])";

/// Minimum List-Id-bearing rows before an all-zero unsubscribe count is
/// treated as server-side filtering rather than a genuinely list-free
/// mailbox. A false positive only costs one bounded full-header refetch —
/// the refetched flags are ground truth either way.
pub(crate) const QUIRK_MIN_LIST_ID_ROWS: usize = 25;

/// Whether a scanned mailbox looks like the server filtered the
/// List-Unsubscribe pair out of HEADER.FIELDS responses: plenty of rows
/// carry a List-Id (bulk mail is present), yet not a single row anywhere in
/// the mailbox has either unsubscribe header.
pub(crate) fn header_fields_quirk(list_id_rows: usize, unsubscribe_rows: usize) -> bool {
    list_id_rows >= QUIRK_MIN_LIST_ID_ROWS && unsubscribe_rows == 0
}

/// Slice convenience for [`header_fields_quirk`].
pub(crate) fn header_fields_look_filtered(rows: &[ListHeaderRow]) -> bool {
    let list_id_rows = rows.iter().filter(|row| row.list_id.is_some()).count();
    let unsubscribe_rows = rows
        .iter()
        .filter(|row| row.list_unsubscribe.is_some() || row.list_unsubscribe_post.is_some())
        .count();
    header_fields_quirk(list_id_rows, unsubscribe_rows)
}

/// Fetch the unified immutable ranking-header projection for explicit UIDs in
/// an already examined or selected mailbox. Every returned UID produces a row,
/// including malformed or non-list messages.
pub async fn fetch_rank_headers_for_uids(
    session: &mut ImapSession,
    uids: &[u32],
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<ListHeaderRow>> {
    fetch_rank_header_rows(session, uids, RANK_HEADER_FIELDS_ITEMS, on_progress, cancel).await
}

/// Like [`fetch_rank_headers_for_uids`], but requests the complete header
/// block — the fallback for HEADER.FIELDS-filtering servers. Same parsing,
/// same row shape; only raw bytes on the wire differ.
pub async fn fetch_rank_headers_full_for_uids(
    session: &mut ImapSession,
    uids: &[u32],
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<ListHeaderRow>> {
    fetch_rank_header_rows(session, uids, RANK_HEADER_FULL_ITEMS, on_progress, cancel).await
}

async fn fetch_rank_header_rows(
    session: &mut ImapSession,
    uids: &[u32],
    items: &str,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<Vec<ListHeaderRow>> {
    let total = uids.len() as u64;
    let mut results = Vec::with_capacity(uids.len());
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
        check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetched = timed_uid_fetch_collect(session, &uid_set, items).await?;

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let uid = match fetch.uid {
                Some(u) => u,
                None => continue,
            };
            let header_bytes = fetch.header().unwrap_or(&[]);
            let header_str = String::from_utf8_lossy(header_bytes);

            let list_unsub = extract_header_value(&header_str, "List-Unsubscribe");
            let list_id = extract_header_value(&header_str, "List-Id");
            let list_unsub_post = extract_header_value(&header_str, "List-Unsubscribe-Post");

            let (sender_email, sender_name, date, message_id) =
                parser::parse_sender_date(header_bytes).unwrap_or_default();

            results.push(ListHeaderRow {
                uid,
                uid_validity: None,
                list_unsubscribe: list_unsub,
                list_unsubscribe_post: list_unsub_post,
                list_id,
                sender_email,
                sender_name,
                date,
                message_id,
            });
        }

        completed += chunk.len() as u64;
        if let Some(progress) = on_progress {
            progress(completed, total);
        }
    }

    Ok(results)
}

/// Search for UIDs at or above `from_uid` in the selected mailbox. Filters out
/// the `*`-range quirk (an empty `from:*` range returns the highest UID).
pub async fn search_uids_from(session: &mut ImapSession, from_uid: u32) -> Result<Vec<u32>> {
    let query = format!("UID {from_uid}:*");
    let uids = run_uid_search(session, &query).await?;
    Ok(uids.into_iter().filter(|&u| u >= from_uid).collect())
}

/// Fetch the raw `List-Id` header for a set of UIDs (already-selected mailbox).
/// Returns `(uid, Option<List-Id value>)`. Used to confirm an exact List-Id
/// match before deletion, since IMAP `HEADER` search is substring-only.
pub async fn fetch_list_ids_for_uids(
    session: &mut ImapSession,
    uids: &[u32],
) -> Result<Vec<(u32, Option<String>)>> {
    fetch_list_ids_for_uids_cancellable(session, uids, None).await
}

/// Cancellable List-Id projection fetch for large cleanup candidate sets.
pub async fn fetch_list_ids_for_uids_cancellable<T>(
    session: &mut Session<T>,
    uids: &[u32],
    cancel: Option<&CancelFn>,
) -> Result<Vec<(u32, Option<String>)>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let mut results = Vec::with_capacity(uids.len());
    for chunk in uids.chunks(1000) {
        check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetched = timed_uid_fetch_collect(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (List-Id)])",
        )
        .await?;
        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let Some(uid) = fetch.uid else { continue };
            let header_str = String::from_utf8_lossy(fetch.header().unwrap_or(&[]));
            results.push((uid, extract_header_value(&header_str, "List-Id")));
        }
    }
    check_cancel(cancel)?;
    Ok(results)
}

/// Fetch specific UIDs and parse them into MessageInfo.
pub async fn fetch_by_uids(
    session: &mut ImapSession,
    uids: &[u32],
    mailbox: &str,
    account_name: &str,
    include_content: bool,
    include_headers: bool,
) -> Result<Vec<MessageInfo>> {
    let uid_set: String = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");

    // RFC 3501 requires parentheses around multiple fetch attributes.
    // async_imap does not add them automatically.
    let fetch_items = if include_content {
        format!(
            "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.{}>)",
            MAX_TRANSIENT_MESSAGE_BYTES + 1
        )
    } else {
        "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])".to_string()
    };

    debug!(
        uid_count = uids.len(),
        include_content, include_headers, "UID FETCH request"
    );

    let fetched = timed_uid_fetch_collect(session, &uid_set, &fetch_items).await?;

    debug!(stream_items = fetched.len(), "UID FETCH stream collected");

    // Extract owned data from the IMAP fetch results so we can parse off-thread
    let mut raw_items: RawFetchItems = Vec::with_capacity(fetched.len());
    for item in fetched {
        if item.is_err() {
            debug!("FETCH item error");
        }
        let fetch = item.map_err(AgentmailError::Imap)?;
        let uid = fetch.uid.unwrap_or(0);
        let size = fetch.size;
        if include_content && size.is_none_or(|value| value as usize > MAX_TRANSIENT_MESSAGE_BYTES)
        {
            return Err(AgentmailError::Other(format!(
                "message UID {uid} is missing a usable RFC822.SIZE or exceeds the {} byte transient fetch limit",
                MAX_TRANSIENT_MESSAGE_BYTES
            )));
        }
        let flags: Vec<String> = fetch.flags().map(|f| flag_to_string(&f)).collect();
        let raw = if include_content {
            fetch.body().unwrap_or(&[])
        } else {
            fetch.header().unwrap_or(&[])
        };
        if raw.len() > MAX_TRANSIENT_MESSAGE_BYTES {
            return Err(AgentmailError::Other(format!(
                "message UID {uid} exceeds the {} byte transient fetch limit",
                MAX_TRANSIENT_MESSAGE_BYTES
            )));
        }
        raw_items.push((uid, size, flags, raw.to_vec()));
    }

    // Parse all messages on a blocking thread (CPU-intensive MIME + HTML→markdown)
    let mailbox = mailbox.to_string();
    let account_name = account_name.to_string();
    let uid_order: Vec<u32> = uids.to_vec();
    let messages = tokio::task::spawn_blocking(move || -> Result<Vec<MessageInfo>> {
        let mut msgs = Vec::with_capacity(raw_items.len());
        for (uid, size, flags, raw) in raw_items {
            let msg = parser::parse_rfc822(
                &raw,
                uid,
                flags,
                size,
                &mailbox,
                &account_name,
                include_content,
                include_headers,
            )?;
            msgs.push(msg);
        }
        // Preserve the requested UID order (newest first)
        msgs.sort_by(|a, b| {
            let pos_a = uid_order
                .iter()
                .position(|u| *u == a.uid)
                .unwrap_or(usize::MAX);
            let pos_b = uid_order
                .iter()
                .position(|u| *u == b.uid)
                .unwrap_or(usize::MAX);
            pos_a.cmp(&pos_b)
        });
        Ok(msgs)
    })
    .await
    .map_err(|e| AgentmailError::Other(format!("spawn_blocking join error: {}", e)))??;

    Ok(messages)
}

// ---------------------------------------------------------------------------
// Flag operations
// ---------------------------------------------------------------------------

/// Get current flags for a single message by UID.
/// Caller must have already selected the mailbox.
pub async fn get_flags(session: &mut ImapSession, uid: u32) -> Result<Vec<String>> {
    let uid_str = uid.to_string();
    let fetched = timed_uid_fetch_collect(session, &uid_str, "(FLAGS)").await?;
    let fetch = fetched
        .into_iter()
        .next()
        .ok_or(AgentmailError::MessageNotFound(uid))?
        .map_err(AgentmailError::Imap)?;
    Ok(fetch.flags().map(|f| flag_to_string(&f)).collect())
}

/// Replace all flags on a message (STORE with FLAGS, not +FLAGS/-FLAGS).
/// Caller must have already selected the mailbox.
pub async fn set_flags(session: &mut ImapSession, uid: u32, flags: &[String]) -> Result<()> {
    let uid_str = uid.to_string();
    let flag_list = flags.join(" ");
    let store_item = format!("FLAGS ({})", flag_list);
    imap_timeout(async {
        let _: Vec<_> = session
            .uid_store(&uid_str, &store_item)
            .await?
            .collect::<Vec<_>>()
            .await;
        Ok::<_, async_imap::error::Error>(())
    })
    .await
}

/// Add flags to a message (STORE with +FLAGS).
/// Caller must have already selected the mailbox.
pub async fn add_flags(session: &mut ImapSession, uid: u32, flags: &[String]) -> Result<()> {
    let uid_str = uid.to_string();
    let flag_list = flags.join(" ");
    let store_item = format!("+FLAGS ({})", flag_list);
    imap_timeout(async {
        let _: Vec<_> = session
            .uid_store(&uid_str, &store_item)
            .await?
            .collect::<Vec<_>>()
            .await;
        Ok::<_, async_imap::error::Error>(())
    })
    .await
}

/// Remove flags from a message (STORE with -FLAGS).
/// Caller must have already selected the mailbox.
pub async fn remove_flags(session: &mut ImapSession, uid: u32, flags: &[String]) -> Result<()> {
    let uid_str = uid.to_string();
    let flag_list = flags.join(" ");
    let store_item = format!("-FLAGS ({})", flag_list);
    imap_timeout(async {
        let _: Vec<_> = session
            .uid_store(&uid_str, &store_item)
            .await?
            .collect::<Vec<_>>()
            .await;
        Ok::<_, async_imap::error::Error>(())
    })
    .await
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

/// Flush pending server-side state after a mutation (EXPUNGE, EXISTS, etc.).
/// Issues NOOP which forces the server to send any queued untagged responses,
/// ensuring the session view is up-to-date before release back to the pool.
pub async fn sync<T>(session: &mut Session<T>) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    imap_timeout(session.noop()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Attachment detection via Content-Type header
// ---------------------------------------------------------------------------

/// Mailbox-local attachment hit with enough metadata for deterministic
/// account-wide ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentUid {
    pub uid: u32,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
}

fn compare_attachment_newest_first(a: &AttachmentUid, b: &AttachmentUid) -> std::cmp::Ordering {
    b.date.cmp(&a.date).then_with(|| b.uid.cmp(&a.uid))
}

/// Fetch UIDs and internal dates of messages that have attachments.
/// Uses lightweight Content-Type header check: multipart/mixed indicates attachments.
/// Returns hits sorted newest-first.
pub async fn fetch_attachment_uids(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<(Vec<AttachmentUid>, u32)> {
    let mb = imap_timeout(session.select(mailbox)).await?;
    let uid_validity = require_uid_validity(mailbox, mb.uid_validity)?;
    if mb.exists == 0 {
        return Ok((Vec::new(), uid_validity));
    }

    let uids = search_all_uids_checked(session, mb.exists, mb.uid_next).await?;
    let total = uids.len() as u64;
    let mut attachment_uids = Vec::new();
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
        check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetched = timed_uid_fetch_collect(
            session,
            &uid_set,
            "(UID INTERNALDATE BODY.PEEK[HEADER.FIELDS (Content-Type)])",
        )
        .await?;

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let uid = fetch.uid.unwrap_or(0);
            if uid == 0 {
                continue;
            }
            let header_bytes = fetch.header().unwrap_or(&[]);
            let header_str = String::from_utf8_lossy(header_bytes).to_lowercase();
            if header_str.contains("multipart/mixed") || header_str.contains("multipart/related") {
                attachment_uids.push(AttachmentUid {
                    uid,
                    date: fetch
                        .internal_date()
                        .map(|date| date.with_timezone(&chrono::Utc)),
                });
            }
        }

        completed += chunk.len() as u64;
        if let Some(progress) = on_progress {
            progress(completed, total);
        }
    }

    attachment_uids.sort_unstable_by(compare_attachment_newest_first);
    Ok((attachment_uids, uid_validity))
}

// ---------------------------------------------------------------------------
// Delete operations
// ---------------------------------------------------------------------------

/// Result of bulk deletion: (deleted UIDs, failed UIDs, trash_fallback).
/// `trash_fallback` is true when trash MOVE failed and we fell back to flag+expunge.
pub struct BulkDeleteResult {
    pub deleted: Vec<u32>,
    pub failed: Vec<u32>,
    pub trash_fallback: bool,
}

/// Policy-aware bulk deletion. When `allow_permanent_fallback` is false, a
/// failed MOVE/COPY-to-Trash remains a failed chunk and can never escalate to
/// an irreversible UID EXPUNGE. Gmail never permits this fallback regardless
/// of caller authorization because in-place EXPUNGE has label semantics there.
pub async fn bulk_delete_messages_with_policy<T>(
    session: &mut Session<T>,
    uids: &[u32],
    trash_mailbox: Option<&str>,
    caps: &ServerCaps,
    allow_permanent_fallback: bool,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<BulkDeleteResult>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    if trash_mailbox.is_some() && !caps.has_move() && !caps.has_uidplus() {
        return Err(AgentmailError::Other(
            "server supports neither MOVE nor UIDPLUS; refusing an unsafe COPY + plain EXPUNGE Trash emulation"
                .to_string(),
        ));
    }
    // On Gmail, \Deleted + EXPUNGE in a label folder only removes that label —
    // the message survives in All Mail. A real delete must move to
    // [Gmail]/Trash, so refuse in-place expunge here (callers resolve Trash for
    // Gmail even in permanent mode; this guards the can't-resolve-Trash case).
    if trash_mailbox.is_none() && caps.is_gmail() {
        return Err(AgentmailError::Other(
            "Gmail: deletes must move to [Gmail]/Trash (in-place EXPUNGE only \
             removes a label); could not resolve the Trash mailbox"
                .to_string(),
        ));
    }
    // A permanent delete (no trash, or trash fallback) requires UIDPLUS: plain
    // EXPUNGE would purge every \Deleted message in the mailbox, including ones
    // flagged by other clients. Refuse up-front rather than risk data loss.
    if trash_mailbox.is_none() && !caps.has_uidplus() {
        return Err(AgentmailError::Other(
            "server lacks UIDPLUS; refusing permanent delete because plain EXPUNGE \
             would remove unrelated \\Deleted messages"
                .to_string(),
        ));
    }

    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut trash_fallback = false;
    let mut use_trash = trash_mailbox;
    let total = uids.len() as u64;

    for chunk in uids.chunks(500) {
        check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let result: std::result::Result<(), AgentmailError> = if let Some(trash) = use_trash {
            match move_uids(session, &uid_set, trash, caps).await {
                Ok(()) => Ok(()),
                Err(_) if can_fallback_from_trash_to_permanent(caps, allow_permanent_fallback) => {
                    // Trash move failed — fall back to permanent delete for all
                    // remaining chunks (safe: non-Gmail + UIDPLUS confirmed).
                    trash_fallback = true;
                    use_trash = None;
                    flag_and_expunge(session, &uid_set).await
                }
                Err(e) => Err(e),
            }
        } else {
            flag_and_expunge(session, &uid_set).await
        };

        match result {
            Ok(()) => {
                deleted.extend_from_slice(chunk);
                let _ = imap_timeout(session.noop()).await;
            }
            Err(_) => failed.extend_from_slice(chunk),
        }

        if let Some(progress) = on_progress {
            progress((deleted.len() + failed.len()) as u64, total);
        }
    }

    Ok(BulkDeleteResult {
        deleted,
        failed,
        trash_fallback,
    })
}

fn can_fallback_from_trash_to_permanent(caps: &ServerCaps, allow_permanent_fallback: bool) -> bool {
    allow_permanent_fallback && caps.has_uidplus() && !caps.is_gmail()
}

/// Move a UID set to `destination`, using MOVE when the server advertises it
/// (RFC 6851) or emulating with COPY + `\Deleted` + UID EXPUNGE otherwise.
/// The emulation path requires UIDPLUS (callers gate on it).
async fn move_uids<T>(
    session: &mut Session<T>,
    uid_set: &str,
    destination: &str,
    caps: &ServerCaps,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    if caps.has_move() {
        imap_timeout(session.uid_mv(uid_set, destination)).await?;
        Ok(())
    } else {
        imap_timeout(session.uid_copy(uid_set, destination)).await?;
        flag_and_expunge(session, uid_set).await
    }
}

/// Flag messages as deleted and expunge them (permanent delete). Uses UID
/// EXPUNGE (RFC 4315) — callers must have confirmed UIDPLUS.
async fn flag_and_expunge<T>(
    session: &mut Session<T>,
    uid_set: &str,
) -> std::result::Result<(), AgentmailError>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    imap_timeout(async {
        let _: Vec<_> = session
            .uid_store(uid_set, "+FLAGS (\\Deleted)")
            .await?
            .collect::<Vec<_>>()
            .await;
        let _: Vec<_> = session
            .uid_expunge(uid_set)
            .await?
            .collect::<Vec<_>>()
            .await;
        Ok::<_, async_imap::error::Error>(())
    })
    .await
}

// ---------------------------------------------------------------------------
// Move
// ---------------------------------------------------------------------------

/// Move a message to another mailbox by UID. Uses MOVE when available, else
/// COPY + `\Deleted` + UID EXPUNGE (which needs UIDPLUS).
pub async fn move_message(
    session: &mut ImapSession,
    uid: u32,
    destination: &str,
    caps: &ServerCaps,
) -> Result<()> {
    if !caps.has_move() && !caps.has_uidplus() {
        return Err(AgentmailError::Other(
            "server supports neither MOVE nor UIDPLUS; cannot move messages safely".to_string(),
        ));
    }
    move_uids(session, &uid.to_string(), destination, caps).await
}

// ---------------------------------------------------------------------------
// Append (drafts)
// ---------------------------------------------------------------------------

/// Append an RFC822 message to a mailbox with the \Draft flag.
pub async fn append_draft(
    session: &mut ImapSession,
    drafts_mailbox: &str,
    rfc822_message: &[u8],
) -> Result<()> {
    imap_timeout(session.append(drafts_mailbox, Some(r"(\Draft)"), None, rfc822_message)).await?;
    Ok(())
}

/// Locate a just-APPENDed message by Message-ID, returning the mailbox
/// UIDVALIDITY and the matching UID (the highest, should duplicates exist).
/// Used to recover a new draft's identity, since async-imap does not expose
/// the UIDPLUS APPENDUID response code.
pub async fn find_uid_by_message_id(
    session: &mut ImapSession,
    mailbox: &str,
    message_id: &str,
) -> Result<Option<(u32, u32)>> {
    let selected = examine(session, mailbox).await?;
    let uid_validity = require_uid_validity(mailbox, selected.uid_validity)?;
    let uids = search_by_header(session, "Message-ID", message_id).await?;
    Ok(uids.into_iter().max().map(|uid| (uid_validity, uid)))
}

// ---------------------------------------------------------------------------
// Raw source
// ---------------------------------------------------------------------------

/// Fetch the raw RFC822 source of a single live-validated message identity.
pub async fn get_message_source<T>(
    session: &mut Session<T>,
    mailbox: &str,
    uid: u32,
    expected_uid_validity: u32,
) -> Result<Vec<u8>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    get_message_source_bounded(
        session,
        mailbox,
        uid,
        expected_uid_validity,
        MAX_TRANSIENT_MESSAGE_BYTES,
    )
    .await
}

/// Fetch one complete message after a size preflight, without ever requesting
/// more than `max_bytes + 1` octets from the server.
pub(crate) async fn get_message_source_bounded<T>(
    session: &mut Session<T>,
    mailbox: &str,
    uid: u32,
    expected_uid_validity: u32,
    max_bytes: usize,
) -> Result<Vec<u8>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    examine_with_expected_uid_validity(session, mailbox, expected_uid_validity).await?;
    let uid_str = uid.to_string();
    let metadata = timed_uid_fetch_collect(session, &uid_str, "(UID RFC822.SIZE)").await?;
    let mut size = None;
    for item in metadata {
        let fetch = item.map_err(AgentmailError::Imap)?;
        if fetch.uid == Some(uid) {
            size = fetch.size.map(|value| value as usize);
            break;
        }
    }
    let size = size.ok_or(AgentmailError::MessageNotFound(uid))?;
    if size > max_bytes {
        return Err(AgentmailError::Other(format!(
            "message UID {uid} is {size} bytes; this resource is limited to {max_bytes} bytes"
        )));
    }

    let fetch_items = format!("(UID BODY.PEEK[]<0.{}>)", max_bytes.saturating_add(1));
    let fetched = timed_uid_fetch_collect(session, &uid_str, &fetch_items).await?;

    let mut body = None;
    for item in fetched {
        let fetch = item.map_err(AgentmailError::Imap)?;
        if fetch.uid == Some(uid) {
            body = fetch.body().map(<[u8]>::to_vec);
            break;
        }
    }
    let body = body.ok_or(AgentmailError::MessageNotFound(uid))?;
    if body.len() > max_bytes || body.len() != size {
        return Err(AgentmailError::Other(format!(
            "message UID {uid} source was truncated or exceeded the {max_bytes} byte resource limit"
        )));
    }
    Ok(body)
}

/// Fetch the exact RFC822 header block with a bounded partial BODY.PEEK.
pub(crate) async fn get_message_headers_bounded<T>(
    session: &mut Session<T>,
    mailbox: &str,
    uid: u32,
    expected_uid_validity: u32,
    max_bytes: usize,
) -> Result<Vec<u8>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    examine_with_expected_uid_validity(session, mailbox, expected_uid_validity).await?;
    let fetch_items = format!("(UID BODY.PEEK[HEADER]<0.{}>)", max_bytes.saturating_add(1));
    let fetched = timed_uid_fetch_collect(session, &uid.to_string(), &fetch_items).await?;
    let mut headers = None;
    for item in fetched {
        let fetch = item.map_err(AgentmailError::Imap)?;
        if fetch.uid == Some(uid) {
            headers = fetch.header().or_else(|| fetch.body()).map(<[u8]>::to_vec);
            break;
        }
    }
    let headers = headers.ok_or(AgentmailError::MessageNotFound(uid))?;
    if headers.len() > max_bytes {
        return Err(AgentmailError::Other(format!(
            "message UID {uid} headers exceed the {max_bytes} byte resource limit"
        )));
    }
    Ok(headers)
}

// ---------------------------------------------------------------------------
// Unsubscribe helpers
// ---------------------------------------------------------------------------

/// Fetch unsubscribe-related headers for a single message.
/// Headers extracted from a message for unsubscribe handling.
pub struct UnsubscribeHeaders {
    pub list_unsubscribe: Option<String>,
    pub list_unsubscribe_post: Option<String>,
    pub list_id: Option<String>,
}

/// Transient, live-validated source used for RFC 8058 execution. The raw
/// message is never cached; it is needed only to verify the DKIM body hash.
pub struct UnsubscribeTarget {
    pub uid_validity: u32,
    pub raw_message: Vec<u8>,
}

fn validate_unsubscribe_message_size(uid: u32, size: Option<u32>) -> Result<u32> {
    let size = size.ok_or_else(|| {
        AgentmailError::Other(format!(
            "message UID {uid} did not provide RFC822.SIZE; refusing an unbounded DKIM source fetch"
        ))
    })?;
    if size > MAX_UNSUBSCRIBE_MESSAGE_BYTES {
        return Err(AgentmailError::Other(format!(
            "message UID {uid} is {size} bytes; one-click DKIM verification is limited to {MAX_UNSUBSCRIBE_MESSAGE_BYTES} bytes"
        )));
    }
    Ok(size)
}

/// EXAMINE a mailbox, enforce the caller's UIDVALIDITY epoch, and fetch the
/// complete target with BODY.PEEK so DKIM can be verified without setting
/// `\\Seen`. A size-only preflight and bounded partial fetch prevent one
/// malformed or oversized message from causing an unbounded allocation.
pub async fn fetch_unsubscribe_target<T>(
    session: &mut Session<T>,
    mailbox: &str,
    uid: u32,
    expected_uid_validity: u32,
    cancel: Option<&CancelFn>,
) -> Result<UnsubscribeTarget>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    check_cancel(cancel)?;
    examine_with_expected_uid_validity(session, mailbox, expected_uid_validity).await?;

    let uid_set = uid.to_string();
    check_cancel(cancel)?;
    let metadata = timed_uid_fetch_collect(session, &uid_set, "(UID RFC822.SIZE)").await?;
    let mut expected_size = None;
    for item in metadata {
        let fetch = item.map_err(AgentmailError::Imap)?;
        if fetch.uid == Some(uid) {
            expected_size = Some(validate_unsubscribe_message_size(uid, fetch.size)?);
            break;
        }
    }
    let expected_size = expected_size.ok_or(AgentmailError::MessageNotFound(uid))?;

    check_cancel(cancel)?;
    // Request at most limit+1 octets. If a server understates RFC822.SIZE, the
    // returned-length equality check below still rejects a truncated source;
    // this matters for DKIM signatures that use an `l=` body-length tag.
    let body_items = format!("(UID BODY.PEEK[]<0.{}>)", MAX_UNSUBSCRIBE_MESSAGE_BYTES + 1);
    let fetched = timed_uid_fetch_collect(session, &uid_set, &body_items).await?;
    let mut raw_message = None;
    for item in fetched {
        let fetch = item.map_err(AgentmailError::Imap)?;
        if fetch.uid == Some(uid) {
            raw_message = fetch.body().map(<[u8]>::to_vec);
            break;
        }
    }
    let raw_message = raw_message.ok_or(AgentmailError::MessageNotFound(uid))?;
    // Yahoo/AOL's RFC822.SIZE is unreliable metadata — observed deltas run
    // in BOTH directions (line-ending/charset accounting at ingestion), so a
    // mismatch is not evidence of truncation. The only case where a clipped
    // body could still verify is a DKIM signature carrying an `l=`
    // body-length tag (RFC 6376 §3.5, widely discouraged); refuse the
    // mismatch then. Otherwise proceed — DKIM verification over the received
    // bytes is the cryptographic integrity check.
    if raw_message.len() != expected_size as usize {
        if dkim_signature_uses_body_length(&raw_message) {
            return Err(AgentmailError::Other(format!(
                "message UID {uid} changed size during DKIM fetch (expected {expected_size} bytes, received {}) and a DKIM signature uses an l= body-length tag; refusing the unsafe combination",
                raw_message.len()
            )));
        }
        debug!(
            target: "agentmail",
            uid,
            expected = expected_size,
            received = raw_message.len(),
            "server RFC822.SIZE disagrees with delivered bytes; proceeding — DKIM verification arbitrates integrity"
        );
    }
    check_cancel(cancel)?;

    Ok(UnsubscribeTarget {
        uid_validity: expected_uid_validity,
        raw_message,
    })
}

pub async fn fetch_unsubscribe_headers<T>(
    session: &mut Session<T>,
    mailbox: &str,
    uid: u32,
    expected_uid_validity: u32,
) -> Result<UnsubscribeHeaders>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    examine_with_expected_uid_validity(session, mailbox, expected_uid_validity).await?;
    let uid_str = uid.to_string();
    let fetched = timed_uid_fetch_collect(
        session,
        &uid_str,
        "BODY.PEEK[HEADER.FIELDS (List-Unsubscribe List-Unsubscribe-Post List-Id)]",
    )
    .await?;

    let fetch = fetched
        .into_iter()
        .next()
        .ok_or(AgentmailError::MessageNotFound(uid))?
        .map_err(AgentmailError::Imap)?;

    let header_bytes = fetch.header().unwrap_or(&[]);
    let header_str = String::from_utf8_lossy(header_bytes);

    Ok(UnsubscribeHeaders {
        list_unsubscribe: extract_header_value(&header_str, "List-Unsubscribe"),
        list_unsubscribe_post: extract_header_value(&header_str, "List-Unsubscribe-Post"),
        list_id: extract_header_value(&header_str, "List-Id"),
    })
}

/// Search for messages matching a specific header name/value pair.
pub async fn search_by_header(
    session: &mut ImapSession,
    header_name: &str,
    header_value: &str,
) -> Result<Vec<u32>> {
    let mut query = format!("HEADER {} {}", quoted(header_name)?, quoted(header_value)?);
    if !header_name.is_ascii() || !header_value.is_ascii() {
        query = format!("CHARSET UTF-8 {query}");
    }
    run_uid_search(session, &query).await
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Quote a string for use in an IMAP SEARCH command.
///
/// Rejects CR/LF outright: async-imap writes command bytes to the wire
/// unvalidated, so an embedded CRLF would inject a second IMAP command.
fn quoted(s: &str) -> Result<String> {
    if s.contains('\r') || s.contains('\n') {
        return Err(AgentmailError::InvalidSearch(
            "search text must not contain CR or LF characters".to_string(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Build an IMAP SEARCH query string from SearchCriteria.
///
/// Non-ASCII text gets a `CHARSET UTF-8` prefix with UTF-8 bytes inside
/// quoted strings. Strictly RFC 3501 wants literals for 8-bit data, but
/// async-imap cannot send command literals; UTF-8-in-quoted is accepted by
/// Gmail, Dovecot, Courier, iCloud, and Outlook, and IMAP4rev2 requires it.
/// Servers that refuse reply with NO [BADCHARSET], mapped to a clear error
/// in `run_uid_search`.
fn build_search_query(criteria: &SearchCriteria) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut ascii_only = true;
    let mut push_text = |key: &str, value: &str, parts: &mut Vec<String>| -> Result<()> {
        ascii_only &= value.is_ascii();
        parts.push(format!("{key} {}", quoted(value)?));
        Ok(())
    };

    if let Some(ref text) = criteria.text {
        push_text("TEXT", text, &mut parts)?;
    }
    if let Some(ref from) = criteria.from {
        push_text("FROM", from, &mut parts)?;
    }
    if let Some(ref subject) = criteria.subject {
        push_text("SUBJECT", subject, &mut parts)?;
    }
    if let Some(ref to) = criteria.to {
        push_text("TO", to, &mut parts)?;
    }
    if let Some(seen) = criteria.seen {
        parts.push(if seen { "SEEN".into() } else { "UNSEEN".into() });
    }
    if let Some(flagged) = criteria.flagged {
        parts.push(if flagged {
            "FLAGGED".into()
        } else {
            "UNFLAGGED".into()
        });
    }
    if let Some(deleted) = criteria.deleted {
        parts.push(if deleted {
            "DELETED".into()
        } else {
            "UNDELETED".into()
        });
    }
    if let Some((ref key, ref value)) = criteria.header {
        ascii_only &= key.is_ascii() && value.is_ascii();
        parts.push(format!("HEADER {} {}", quoted(key)?, quoted(value)?));
    }
    // IMAP date format is `dd-Mon-yyyy` (e.g. 01-Jan-2024), always ASCII.
    if let Some(since) = criteria.since {
        parts.push(format!("SINCE {}", since.format("%d-%b-%Y")));
    }
    if let Some(before) = criteria.before {
        parts.push(format!("BEFORE {}", before.format("%d-%b-%Y")));
    }
    if let Some(n) = criteria.larger_than {
        parts.push(format!("LARGER {n}"));
    }
    if let Some(n) = criteria.smaller_than {
        parts.push(format!("SMALLER {n}"));
    }

    if parts.is_empty() {
        Ok("ALL".to_string())
    } else if ascii_only {
        Ok(parts.join(" "))
    } else {
        Ok(format!("CHARSET UTF-8 {}", parts.join(" ")))
    }
}

/// Run a UID SEARCH, mapping server rejections of the query itself to
/// `InvalidSearch` so callers see an actionable error instead of a generic
/// IMAP failure.
async fn run_uid_search<T>(session: &mut Session<T>, query: &str) -> Result<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    match imap_timeout(session.uid_search(query)).await {
        Ok(uids) => Ok(uids.into_iter().collect()),
        Err(AgentmailError::Imap(async_imap::error::Error::Bad(s))) => Err(
            AgentmailError::InvalidSearch(format!("server rejected SEARCH: {s}")),
        ),
        Err(AgentmailError::Imap(async_imap::error::Error::No(s)))
            if s.to_uppercase().contains("BADCHARSET") =>
        {
            Err(AgentmailError::InvalidSearch(format!(
                "server does not support UTF-8 SEARCH: {s}"
            )))
        }
        Err(e) => Err(e),
    }
}

/// Fetch all unique flags in use across messages in a mailbox, with counts.
/// Result of scanning flags in a mailbox: per-flag counts and per-message color resolution.
pub struct FlagScanResult {
    pub flags: Vec<(String, u32)>,
    pub colors: Vec<(String, u32)>,
}

pub async fn fetch_flags(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
    cancel: Option<&CancelFn>,
) -> Result<FlagScanResult> {
    let mb = imap_timeout(session.select(mailbox)).await?;
    if mb.exists == 0 {
        return Ok(FlagScanResult {
            flags: Vec::new(),
            colors: Vec::new(),
        });
    }

    let uids = search_all_uids_checked(session, mb.exists, mb.uid_next).await?;
    let total = uids.len() as u64;
    let mut flag_counts: hashbrown::HashMap<String, u32> = hashbrown::HashMap::new();
    let mut color_counts: hashbrown::HashMap<String, u32> = hashbrown::HashMap::new();
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
        check_cancel(cancel)?;
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetched = timed_uid_fetch_collect(session, &uid_set, "(FLAGS)").await?;

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let msg_flags: Vec<String> = fetch.flags().map(|f| flag_to_string(&f)).collect();
            for name in &msg_flags {
                // entry_ref: allocate the owned `String` key only on first sight
                // of a flag — the distinct flag set is tiny, so this loop (once
                // per email) is almost always a hit. See PERF-entry-ref.md.
                *flag_counts.entry_ref(name.as_str()).or_insert(0) += 1;
            }
            // Resolve Apple Mail color from MailFlagBit combinations
            if let Some(color) = crate::bits_to_color(&msg_flags) {
                // `color` is already a borrowed `&str` — pass it directly.
                *color_counts.entry_ref(color).or_insert(0) += 1;
            }
        }

        completed += chunk.len() as u64;
        if let Some(progress) = on_progress {
            progress(completed, total);
        }
    }

    let mut flags: Vec<(String, u32)> = flag_counts.into_iter().collect();
    flags.sort_by_key(|b| std::cmp::Reverse(b.1));
    let mut colors: Vec<(String, u32)> = color_counts.into_iter().collect();
    colors.sort_by_key(|b| std::cmp::Reverse(b.1));
    Ok(FlagScanResult { flags, colors })
}

/// Convert an async-imap Flag to its string representation.
fn flag_to_string(flag: &async_imap::types::Flag<'_>) -> String {
    match flag {
        async_imap::types::Flag::Seen => "\\Seen".to_string(),
        async_imap::types::Flag::Answered => "\\Answered".to_string(),
        async_imap::types::Flag::Flagged => "\\Flagged".to_string(),
        async_imap::types::Flag::Deleted => "\\Deleted".to_string(),
        async_imap::types::Flag::Draft => "\\Draft".to_string(),
        async_imap::types::Flag::Recent => "\\Recent".to_string(),
        async_imap::types::Flag::MayCreate => "\\*".to_string(),
        async_imap::types::Flag::Custom(cow) => cow.to_string(),
    }
}

/// Public wrapper for `timed_uid_fetch_collect`.
pub async fn timed_uid_fetch_collect_pub<T>(
    session: &mut Session<T>,
    uid_set: &str,
    query: &str,
) -> Result<Vec<std::result::Result<async_imap::types::Fetch, async_imap::error::Error>>>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    timed_uid_fetch_collect(session, uid_set, query).await
}

/// Public wrapper for `extract_header_value`.
pub fn extract_header_value_pub(headers: &str, name: &str) -> Option<String> {
    extract_header_value(headers, name)
}

/// Whether any DKIM-Signature header in the raw message advertises an `l=`
/// body-length tag (RFC 6376 §3.5). Parsing is tag-precise — each header
/// value is unfolded and split on `;` — because a naive substring match
/// would false-positive on base64 `b=` data (whose folded chunks can end in
/// `l==`). Splitting on `;` is safe: base64 never contains `;`, so a `b=`
/// value is always a single segment.
fn dkim_signature_uses_body_length(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let mut current: Option<String> = None;
    let mut values: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break; // end of the header block
        }
        if line.starts_with([' ', '\t']) {
            if let Some(value) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some(value) = current.take() {
            values.push(value);
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("DKIM-Signature")
        {
            current = Some(value.trim().to_string());
        }
    }
    values.extend(current);

    values.iter().any(|value| {
        value.split(';').any(|segment| {
            // RFC 6376 permits FWS around the '='; tag names are exact.
            segment
                .trim()
                .strip_prefix('l')
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
    })
}

/// Extract a header value from raw header text by name.
fn extract_header_value(headers: &str, name: &str) -> Option<String> {
    let mut value: Option<String> = None;
    for line in headers.lines() {
        if line.is_empty() {
            break;
        }

        if line.starts_with([' ', '\t']) {
            if let Some(value) = value.as_mut() {
                let continuation = line.trim();
                if !continuation.is_empty() {
                    if !value.is_empty() {
                        value.push(' ');
                    }
                    value.push_str(continuation);
                }
            }
            continue;
        }

        if value.is_some() {
            break;
        }
        let Some((field_name, field_value)) = line.split_once(':') else {
            continue;
        };
        if field_name.eq_ignore_ascii_case(name) {
            value = Some(field_value.trim().to_string());
        }
    }
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    use super::*;

    async fn scripted_unsubscribe_session(
        uid_validity: Option<u32>,
        serve_fetches: bool,
    ) -> (Session<DuplexStream>, tokio::task::JoinHandle<Vec<String>>) {
        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();

            loop {
                let mut command = String::new();
                let read = reader.read_line(&mut command).await.unwrap();
                if read == 0 {
                    break;
                }
                let tag = command.split_whitespace().next().unwrap().to_string();
                commands.push(command.clone());

                if command.contains(" LOGIN ") {
                    writer
                        .write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes())
                        .await
                        .unwrap();
                } else if command.contains(" EXAMINE ") {
                    let validity = uid_validity.map_or_else(String::new, |value| {
                        format!("* OK [UIDVALIDITY {value}] UIDs valid\r\n")
                    });
                    writer
                        .write_all(
                            format!(
                                "* 1 EXISTS\r\n{validity}{tag} OK [READ-ONLY] EXAMINE completed\r\n"
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    if !serve_fetches {
                        break;
                    }
                } else if command.contains("RFC822.SIZE") {
                    writer
                        .write_all(
                            format!(
                                "* 1 FETCH (UID 42 RFC822.SIZE 5)\r\n{tag} OK FETCH completed\r\n"
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                } else if command.contains("BODY.PEEK[]") {
                    writer
                        .write_all(
                            format!(
                                "* 1 FETCH (UID 42 BODY[]<0> {{5}}\r\nabcde)\r\n{tag} OK FETCH completed\r\n"
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    break;
                } else {
                    panic!("unexpected IMAP command: {command:?}");
                }
            }
            commands
        });

        let client = async_imap::Client::new(client_stream);
        let session = client
            .login("test-user", "test-password")
            .await
            .map_err(|(error, _)| error)
            .unwrap();
        (session, server)
    }

    #[test]
    fn special_use_parser_preserves_multiple_roles() {
        use async_imap::types::NameAttribute;

        let attrs = [
            NameAttribute::All,
            NameAttribute::Archive,
            NameAttribute::Extension(Cow::Borrowed("\\Important")),
        ];

        assert_eq!(
            roles_from_attributes(&attrs),
            ["all", "archive", "important"]
        );
    }

    #[test]
    fn rev2_nonexistent_attribute_is_unselectable_case_insensitively() {
        use async_imap::types::NameAttribute;

        for value in ["\\NonExistent", "nonexistent", "\\NONEXISTENT"] {
            let attrs = [NameAttribute::Extension(Cow::Borrowed(value))];
            assert!(attributes_are_unselectable(&attrs));
        }
    }

    #[test]
    fn unrelated_extension_does_not_make_mailbox_unselectable() {
        use async_imap::types::NameAttribute;

        let attrs = [NameAttribute::Extension(Cow::Borrowed("\\HasChildren"))];

        assert!(!attributes_are_unselectable(&attrs));
    }

    #[test]
    fn mailbox_page_filters_unselectable_rows_before_offset_and_limit() {
        let layout = |path: &str, no_select: bool| MailboxLayout {
            path: path.to_string(),
            delimiter: Some("/".to_string()),
            no_select,
            no_inferiors: false,
            roles: Vec::new(),
        };
        let layouts = vec![
            layout("container", true),
            layout("INBOX", false),
            layout("Archive", false),
            layout("ghost", true),
            layout("Sent", false),
        ];

        let (page, total) = selectable_mailbox_page(layouts, 1, 1);

        assert_eq!(total, 3);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].path, "Archive");
    }

    #[test]
    fn attachment_hits_sort_by_date_then_uid_newest_first() {
        let older = "2025-01-01T00:00:00Z".parse().unwrap();
        let newer = "2025-02-01T00:00:00Z".parse().unwrap();
        let mut hits = vec![
            AttachmentUid {
                uid: 99,
                date: Some(older),
            },
            AttachmentUid {
                uid: 2,
                date: Some(newer),
            },
            AttachmentUid {
                uid: 3,
                date: Some(newer),
            },
            AttachmentUid {
                uid: 100,
                date: None,
            },
        ];

        hits.sort_unstable_by(compare_attachment_newest_first);

        assert_eq!(
            hits.into_iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            [3, 2, 99, 100]
        );
    }

    #[test]
    fn header_extraction_unfolds_rfc_continuation_lines() {
        let headers = concat!(
            "List-Unsubscribe: <mailto:leave@example.com>,\r\n",
            " <https://example.com/unsubscribe/token>\r\n",
            "List-Id: Example <list.example.com>\r\n",
            "\r\n",
        );
        assert_eq!(
            extract_header_value(headers, "list-unsubscribe").as_deref(),
            Some("<mailto:leave@example.com>, <https://example.com/unsubscribe/token>")
        );
        assert_eq!(
            extract_header_value(headers, "List-Id").as_deref(),
            Some("Example <list.example.com>")
        );
    }

    #[test]
    fn uidvalidity_guard_rejects_zero_missing_or_changed_epochs() {
        assert_eq!(
            validate_expected_uid_validity("INBOX", 7, Some(7)).unwrap(),
            7
        );
        assert!(validate_expected_uid_validity("INBOX", 0, Some(7)).is_err());
        assert!(matches!(
            require_uid_validity("INBOX", Some(0)),
            Err(AgentmailError::UidValidityUnavailable { .. })
        ));
        assert!(matches!(
            validate_expected_uid_validity("INBOX", 7, None),
            Err(AgentmailError::UidValidityUnavailable { .. })
        ));
        assert!(matches!(
            validate_expected_uid_validity("INBOX", 7, Some(8)),
            Err(AgentmailError::UidValidityChanged {
                expected: 7,
                actual: Some(8),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn source_fetch_stops_before_uid_command_when_epoch_is_missing_or_stale() {
        for actual in [Some(8), None] {
            let (mut session, server) = scripted_unsubscribe_session(actual, false).await;
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                get_message_source(&mut session, "INBOX", 42, 7),
            )
            .await
            .expect("scripted IMAP operation timed out");

            match actual {
                Some(_) => assert!(matches!(
                    result,
                    Err(AgentmailError::UidValidityChanged { .. })
                )),
                None => assert!(matches!(
                    result,
                    Err(AgentmailError::UidValidityUnavailable { .. })
                )),
            }

            drop(session);
            let commands = server.await.unwrap();
            assert_eq!(commands.len(), 2, "UID FETCH must not follow stale EXAMINE");
            assert!(!commands.iter().any(|command| command.contains("UID FETCH")));
        }
    }

    #[tokio::test]
    async fn unsubscribe_imap_transcript_stops_before_fetch_on_stale_uid_epoch() {
        for actual in [Some(8), None] {
            let (mut session, server) = scripted_unsubscribe_session(actual, false).await;
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                fetch_unsubscribe_target(&mut session, "INBOX", 42, 7, None),
            )
            .await
            .expect("scripted IMAP operation timed out");
            match actual {
                Some(_) => assert!(matches!(
                    result,
                    Err(AgentmailError::UidValidityChanged { .. })
                )),
                None => assert!(matches!(
                    result,
                    Err(AgentmailError::UidValidityUnavailable { .. })
                )),
            }
            drop(session);
            let commands = server.await.unwrap();
            assert_eq!(commands.len(), 2, "UID FETCH must not follow stale EXAMINE");
            assert!(commands[1].contains(" EXAMINE "));
            assert!(!commands.iter().any(|command| command.contains("UID FETCH")));
        }
    }

    #[tokio::test]
    async fn unsubscribe_imap_transcript_fetches_bounded_source_after_matching_epoch() {
        let (mut session, server) = scripted_unsubscribe_session(Some(7), true).await;
        let target = tokio::time::timeout(
            Duration::from_secs(2),
            fetch_unsubscribe_target(&mut session, "INBOX", 42, 7, None),
        )
        .await
        .expect("scripted IMAP operation timed out")
        .expect("matching UIDVALIDITY should fetch the target");
        assert_eq!(target.uid_validity, 7);
        assert_eq!(target.raw_message, b"abcde");

        drop(session);
        let commands = server.await.unwrap();
        assert_eq!(commands.len(), 4);
        assert!(commands[1].contains(" EXAMINE "));
        assert!(commands[2].contains("UID FETCH 42 (UID RFC822.SIZE)"));
        assert!(commands[3].contains("UID FETCH 42 (UID BODY.PEEK[]<0.67108865>)"));
    }

    #[test]
    fn unsubscribe_source_size_guard_is_bounded() {
        assert_eq!(validate_unsubscribe_message_size(9, Some(1)).unwrap(), 1);
        assert_eq!(
            validate_unsubscribe_message_size(9, Some(MAX_UNSUBSCRIBE_MESSAGE_BYTES)).unwrap(),
            MAX_UNSUBSCRIBE_MESSAGE_BYTES
        );
        assert!(validate_unsubscribe_message_size(9, None).is_err());
        assert!(
            validate_unsubscribe_message_size(9, Some(MAX_UNSUBSCRIBE_MESSAGE_BYTES + 1)).is_err()
        );
    }

    #[test]
    fn list_id_only_rows_are_not_mistaken_for_subscriptions() {
        let mut row = ListHeaderRow {
            uid: 1,
            uid_validity: Some(10),
            list_unsubscribe: None,
            list_unsubscribe_post: None,
            list_id: Some("list.example.com".to_string()),
            sender_email: String::new(),
            sender_name: String::new(),
            date: None,
            message_id: None,
        };
        assert!(!has_unsubscribe_header(&row));
        row.list_unsubscribe_post = Some("List-Unsubscribe=One-Click".to_string());
        assert!(has_unsubscribe_header(&row));
    }

    #[test]
    fn special_use_parser_recognizes_current_registered_extensions() {
        use async_imap::types::NameAttribute;

        let attrs = [
            NameAttribute::Extension(Cow::Borrowed("\\MEMOS")),
            NameAttribute::Extension(Cow::Borrowed("Scheduled")),
            NameAttribute::Extension(Cow::Borrowed("\\Snoozed")),
        ];

        assert_eq!(
            roles_from_attributes(&attrs),
            ["memos", "scheduled", "snoozed"]
        );
    }

    #[test]
    fn special_use_parser_ignores_structural_and_unknown_attributes() {
        use async_imap::types::NameAttribute;

        let attrs = [
            NameAttribute::NoSelect,
            NameAttribute::Extension(Cow::Borrowed("\\HasChildren")),
            NameAttribute::Extension(Cow::Borrowed("\\VendorThing")),
        ];

        assert!(roles_from_attributes(&attrs).is_empty());
    }

    fn criteria_text(text: &str) -> SearchCriteria {
        SearchCriteria {
            text: Some(text.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn ascii_query_has_no_charset_prefix() {
        let q = build_search_query(&criteria_text("hello world")).unwrap();
        assert_eq!(q, "TEXT \"hello world\"");
    }

    #[test]
    fn non_ascii_query_gets_utf8_charset_prefix() {
        let q = build_search_query(&criteria_text("café")).unwrap();
        assert_eq!(q, "CHARSET UTF-8 TEXT \"café\"");
    }

    #[test]
    fn non_ascii_in_any_field_triggers_charset() {
        let criteria = SearchCriteria {
            from: Some("björn@example.com".to_string()),
            seen: Some(false),
            ..Default::default()
        };
        let q = build_search_query(&criteria).unwrap();
        assert_eq!(q, "CHARSET UTF-8 FROM \"björn@example.com\" UNSEEN");
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        let q = build_search_query(&criteria_text(r#"say "hi" \now"#)).unwrap();
        assert_eq!(q, r#"TEXT "say \"hi\" \\now""#);
    }

    #[test]
    fn crlf_in_search_text_is_rejected() {
        for bad in ["x\r\nA1 EXPUNGE", "line1\nline2", "cr\rhere"] {
            let err = build_search_query(&criteria_text(bad)).unwrap_err();
            assert!(
                matches!(err, AgentmailError::InvalidSearch(_)),
                "expected InvalidSearch for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn empty_criteria_searches_all() {
        let q = build_search_query(&SearchCriteria::default()).unwrap();
        assert_eq!(q, "ALL");
    }

    #[test]
    fn header_pair_is_quoted() {
        let criteria = SearchCriteria {
            header: Some(("List-Id".to_string(), "news.example.com".to_string())),
            ..Default::default()
        };
        let q = build_search_query(&criteria).unwrap();
        assert_eq!(q, "HEADER \"List-Id\" \"news.example.com\"");
    }

    #[test]
    fn flag_criteria_compose() {
        let criteria = SearchCriteria {
            seen: Some(true),
            flagged: Some(false),
            deleted: Some(false),
            ..Default::default()
        };
        let q = build_search_query(&criteria).unwrap();
        assert_eq!(q, "SEEN UNFLAGGED UNDELETED");
    }

    #[test]
    fn date_and_size_criteria_compose() {
        let criteria = SearchCriteria {
            since: chrono::NaiveDate::from_ymd_opt(2024, 1, 5),
            before: chrono::NaiveDate::from_ymd_opt(2024, 12, 31),
            larger_than: Some(1_000_000),
            smaller_than: Some(5_000_000),
            ..Default::default()
        };
        let q = build_search_query(&criteria).unwrap();
        assert_eq!(
            q,
            "SINCE 05-Jan-2024 BEFORE 31-Dec-2024 LARGER 1000000 SMALLER 5000000"
        );
    }

    #[test]
    fn date_filter_combines_with_text_and_stays_ascii() {
        let criteria = SearchCriteria {
            from: Some("news@example.com".to_string()),
            since: chrono::NaiveDate::from_ymd_opt(2023, 6, 1),
            ..Default::default()
        };
        // No CHARSET prefix — date tokens are ASCII.
        let q = build_search_query(&criteria).unwrap();
        assert_eq!(q, "FROM \"news@example.com\" SINCE 01-Jun-2023");
    }

    #[test]
    fn server_caps_detect_gmail_features() {
        // Representative Gmail CAPABILITY tokens.
        let caps = ServerCaps::from_strings(
            [
                "IMAP4rev1",
                "UIDPLUS",
                "MOVE",
                "X-GM-EXT-1",
                "XLIST",
                "CHILDREN",
            ]
            .into_iter()
            .map(String::from),
        );
        assert!(caps.has_imap4rev1());
        assert!(caps.has_uidplus());
        assert!(caps.has_move());
        assert!(caps.is_gmail());
        assert!(!caps.has("CONDSTORE"));
        assert!(!caps.has_condstore());

        // A non-Gmail server isn't flagged as Gmail.
        let dovecot = ServerCaps::from_strings(
            ["IMAP4rev1", "UIDPLUS", "MOVE", "CONDSTORE"]
                .into_iter()
                .map(String::from),
        );
        assert!(!dovecot.is_gmail());
        assert!(dovecot.has_condstore());
    }

    #[test]
    fn gmail_never_allows_in_place_permanent_fallback() {
        let caps = ServerCaps::from_strings(
            ["IMAP4rev1", "UIDPLUS", "MOVE", "X-GM-EXT-1"]
                .into_iter()
                .map(String::from),
        );

        assert!(!can_fallback_from_trash_to_permanent(&caps, true));
    }

    #[test]
    fn non_gmail_allows_authorized_uidplus_permanent_fallback() {
        let caps = ServerCaps::from_strings(
            ["IMAP4rev1", "UIDPLUS", "MOVE"]
                .into_iter()
                .map(String::from),
        );

        assert!(can_fallback_from_trash_to_permanent(&caps, true));
    }

    #[test]
    fn retryable_connect_covers_transient_auth_and_network() {
        use async_imap::error::Error as ImapErr;
        // Transient auth rejection (iCloud/Gmail "rejected then accepted").
        assert!(is_retryable_connect_error(&AgentmailError::Imap(
            ImapErr::No("[AUTHENTICATIONFAILED] Invalid credentials".into())
        )));
        // Transient network drops.
        assert!(is_retryable_connect_error(&AgentmailError::Imap(
            ImapErr::ConnectionLost
        )));
        assert!(is_retryable_connect_error(&AgentmailError::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")
        )));
        // A protocol BAD or a config/credential-resolution error is not retried.
        assert!(!is_retryable_connect_error(&AgentmailError::Imap(
            ImapErr::Bad("syntax".into())
        )));
        assert!(!is_retryable_connect_error(&AgentmailError::Credential(
            "no password".into()
        )));
    }

    #[test]
    fn server_caps_lookup_is_case_insensitive() {
        let caps = ServerCaps::from_strings(["uidplus".to_string(), "MoVe".to_string()]);
        assert!(caps.has_uidplus());
        assert!(caps.has_move());
        assert!(caps.has("UIDPLUS"));
    }

    #[test]
    fn server_caps_rev2_only_lacks_imap4rev1() {
        // An IMAP4rev2-only server advertises IMAP4rev2, not IMAP4rev1.
        let caps = ServerCaps::from_strings(["IMAP4rev2".to_string(), "UIDPLUS".to_string()]);
        assert!(!caps.has_imap4rev1());
        assert!(caps.has("IMAP4REV2"));
        assert!(caps.has_uidplus());
    }

    #[test]
    fn dkim_body_length_detection_is_tag_precise() {
        // A real l= tag, plain and with RFC-legal whitespace around '='.
        assert!(dkim_signature_uses_body_length(
            b"DKIM-Signature: v=1; a=rsa-sha256; l=1234; b=abc\r\n\r\nbody"
        ));
        assert!(dkim_signature_uses_body_length(
            b"DKIM-Signature: v=1; l = 99; b=xyz\r\n\r\nbody"
        ));
        // Folded l= tag on a continuation line.
        assert!(dkim_signature_uses_body_length(
            b"DKIM-Signature: v=1; a=rsa-sha256;\r\n\tl=42; b=abc\r\n\r\nbody"
        ));
        // Second of two signatures carries the tag.
        assert!(dkim_signature_uses_body_length(
            b"DKIM-Signature: v=1; b=a\r\nDKIM-Signature: v=1; l=5; b=c\r\n\r\nbody"
        ));
        // Folded base64 b= data ending in `l==` must NOT false-positive.
        assert!(!dkim_signature_uses_body_length(
            b"DKIM-Signature: v=1; a=rsa-sha256;\r\n b=abcdefghijk\r\n l==; d=x.test\r\n\r\nbody"
        ));
        // `l=` text in the BODY is out of scope.
        assert!(!dkim_signature_uses_body_length(
            b"DKIM-Signature: v=1; b=abc\r\n\r\nl=fake in body"
        ));
        // Tag names are exact: `length=` is not `l=`.
        assert!(!dkim_signature_uses_body_length(
            b"DKIM-Signature: v=1; length=9; b=abc\r\n\r\nbody"
        ));
    }

    #[test]
    fn message_limit_parses_rfc9738_capability() {
        let caps =
            ServerCaps::from_strings(["IMAP4rev1".to_string(), "MESSAGELIMIT=1000".to_string()]);
        assert_eq!(caps.message_limit(), Some(1000));
        assert_eq!(
            ServerCaps::from_strings(["IDLE".to_string()]).message_limit(),
            None
        );
        assert_eq!(
            ServerCaps::from_strings(["MESSAGELIMIT=abc".to_string()]).message_limit(),
            None
        );
    }

    fn quirk_row(uid: u32, list_id: Option<&str>, unsubscribe: Option<&str>) -> ListHeaderRow {
        ListHeaderRow {
            uid,
            uid_validity: None,
            list_unsubscribe: unsubscribe.map(str::to_string),
            list_unsubscribe_post: None,
            list_id: list_id.map(str::to_string),
            sender_email: "sender@example.com".to_string(),
            sender_name: "Sender".to_string(),
            date: None,
            message_id: None,
        }
    }

    #[test]
    fn header_fields_quirk_requires_many_list_ids_and_zero_unsubscribes() {
        assert!(header_fields_quirk(QUIRK_MIN_LIST_ID_ROWS, 0));
        assert!(
            !header_fields_quirk(QUIRK_MIN_LIST_ID_ROWS - 1, 0),
            "small mailboxes stay below the detection threshold"
        );
        assert!(
            !header_fields_quirk(QUIRK_MIN_LIST_ID_ROWS, 1),
            "a single unsubscribe header disproves server-side filtering"
        );
    }

    #[test]
    fn filtered_detection_matches_the_aol_shape() {
        // AOL/Yahoo shape: many List-Id rows, zero List-Unsubscribe anywhere.
        let filtered: Vec<ListHeaderRow> = (0..QUIRK_MIN_LIST_ID_ROWS as u32)
            .map(|uid| quirk_row(uid + 1, Some("list.example.com"), None))
            .collect();
        assert!(header_fields_look_filtered(&filtered));

        let mut healthy = filtered.clone();
        healthy[0].list_unsubscribe = Some("<https://example.com/unsub>".to_string());
        assert!(
            !header_fields_look_filtered(&healthy),
            "any surviving unsubscribe header means HEADER.FIELDS works"
        );
    }

    /// A server whose `UID SEARCH ALL` is rejected with `NO [LIMIT]` — which
    /// async-imap swallows into an empty `Ok` — but which answers bounded
    /// `UID lo:hi` window searches. The AOL/Yahoo RFC 9738 MESSAGELIMIT shape.
    async fn scripted_message_limit_session()
    -> (Session<DuplexStream>, tokio::task::JoinHandle<Vec<String>>) {
        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();
            loop {
                let mut command = String::new();
                let read = reader.read_line(&mut command).await.unwrap();
                if read == 0 {
                    break;
                }
                let tag = command.split_whitespace().next().unwrap().to_string();
                commands.push(command.clone());

                if command.contains(" LOGIN ") {
                    writer
                        .write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes())
                        .await
                        .unwrap();
                } else if command.contains("UID SEARCH UID ") {
                    writer
                        .write_all(
                            format!("* SEARCH 2 3 5 8 13\r\n{tag} OK SEARCH completed\r\n")
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                    break;
                } else if command.contains("UID SEARCH ALL") {
                    // RFC 9738 MESSAGELIMIT rejection; async-imap drops the
                    // tagged NO and hands the caller an empty result.
                    writer
                        .write_all(format!("{tag} NO [LIMIT] Too many messages\r\n").as_bytes())
                        .await
                        .unwrap();
                } else {
                    panic!("unexpected IMAP command: {command:?}");
                }
            }
            commands
        });

        let client = async_imap::Client::new(client_stream);
        let session = client
            .login("test-user", "test-password")
            .await
            .map_err(|(error, _)| error)
            .unwrap();
        (session, server)
    }

    #[tokio::test]
    async fn truncated_search_all_falls_back_to_windowed_rediscovery() {
        let (mut session, server) = scripted_message_limit_session().await;

        let uids = search_all_uids_checked(&mut session, 5, Some(14))
            .await
            .expect("windowed rediscovery should succeed");

        assert_eq!(uids, vec![2, 3, 5, 8, 13]);
        drop(session);
        let commands = server.await.expect("scripted server should finish");
        assert!(
            commands.iter().any(|c| c.contains("UID SEARCH ALL")),
            "the cheap whole-mailbox search runs first: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("UID SEARCH UID 1:13")),
            "short result triggers one bounded window up to UIDNEXT: {commands:?}"
        );
    }

    /// A scripted server that answers `ENABLE` with an `* ENABLED` echo and a
    /// tagged OK, for driving the `enable` primitive without a real server.
    async fn scripted_enable_session(
        reply: &'static str,
    ) -> (Session<DuplexStream>, tokio::task::JoinHandle<Vec<String>>) {
        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();
            loop {
                let mut command = String::new();
                if reader.read_line(&mut command).await.unwrap() == 0 {
                    break;
                }
                let tag = command.split_whitespace().next().unwrap().to_string();
                commands.push(command.clone());
                if command.contains(" LOGIN ") {
                    writer
                        .write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes())
                        .await
                        .unwrap();
                } else if command.contains("ENABLE ") {
                    writer.write_all(reply.as_bytes()).await.unwrap();
                    writer
                        .write_all(format!("{tag} OK ENABLE completed\r\n").as_bytes())
                        .await
                        .unwrap();
                    break;
                } else {
                    panic!("unexpected command: {command:?}");
                }
            }
            commands
        });
        let client = async_imap::Client::new(client_stream);
        let session = client
            .login("test-user", "test-password")
            .await
            .map_err(|(error, _)| error)
            .unwrap();
        (session, server)
    }

    #[tokio::test]
    async fn enable_confirms_the_server_enabled_response() {
        let (mut session, server) = scripted_enable_session("* ENABLED UIDONLY\r\n").await;

        let enabled = enable(&mut session, "UIDONLY")
            .await
            .expect("ENABLE should succeed on a tagged OK");

        assert_eq!(enabled.len(), 1, "one capability echoed: {enabled:?}");
        drop(session);
        let commands = server.await.expect("scripted server should finish");
        assert!(
            commands.iter().any(|c| c.contains("ENABLE UIDONLY")),
            "the ENABLE command reached the server: {commands:?}"
        );
    }

    /// A scripted server that answers `UID FETCH` with a UID-Mode `UIDFETCH`
    /// response (RFC 9586) whose UID data item is omitted — the leading
    /// number is the UID. Exercises the local imap-proto UIDFETCH patch.
    async fn scripted_uidfetch_session()
    -> (Session<DuplexStream>, tokio::task::JoinHandle<Vec<String>>) {
        let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();
            loop {
                let mut command = String::new();
                if reader.read_line(&mut command).await.unwrap() == 0 {
                    break;
                }
                let tag = command.split_whitespace().next().unwrap().to_string();
                commands.push(command.clone());
                if command.contains(" LOGIN ") {
                    writer
                        .write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes())
                        .await
                        .unwrap();
                } else if command.contains("UID FETCH") {
                    // No `UID` item inside the parens — the 42 IS the UID.
                    writer
                        .write_all(
                            format!(
                                "* 42 UIDFETCH (RFC822.SIZE 5)\r\n{tag} OK UID FETCH completed\r\n"
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    break;
                } else {
                    panic!("unexpected command: {command:?}");
                }
            }
            commands
        });
        let client = async_imap::Client::new(client_stream);
        let session = client
            .login("test-user", "test-password")
            .await
            .map_err(|(error, _)| error)
            .unwrap();
        (session, server)
    }

    /// A scripted UID-Mode server that answers PARTIAL windows: two full pages
    /// of `window` UIDFETCH rows, then a short final page (end of mailbox).
    async fn scripted_partial_walk_session(
        window: u32,
    ) -> (Session<DuplexStream>, tokio::task::JoinHandle<Vec<String>>) {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server_stream);
            let mut reader = BufReader::new(reader);
            let mut commands = Vec::new();
            let mut page = 0u32;
            loop {
                let mut command = String::new();
                if reader.read_line(&mut command).await.unwrap() == 0 {
                    break;
                }
                let tag = command.split_whitespace().next().unwrap().to_string();
                commands.push(command.clone());
                if command.contains(" LOGIN ") {
                    writer
                        .write_all(format!("{tag} OK LOGIN completed\r\n").as_bytes())
                        .await
                        .unwrap();
                } else if command.contains("UID FETCH") {
                    // Page 0,1: `window` rows (full). Page 2: 1 row (short → stop).
                    // UIDs descend contiguously so the shrinking range is
                    // deterministic: page 0 → 100..98, page 1 → 97..95, etc.
                    let rows = if page < 2 { window } else { 1 };
                    let top = 100 - page * window;
                    let mut body = String::new();
                    for i in 0..rows {
                        let uid = top - i;
                        body.push_str(&format!("* {uid} UIDFETCH (UID {uid})\r\n"));
                    }
                    body.push_str(&format!("{tag} OK UID FETCH completed\r\n"));
                    writer.write_all(body.as_bytes()).await.unwrap();
                    page += 1;
                } else {
                    panic!("unexpected command: {command:?}");
                }
            }
            commands
        });
        let client = async_imap::Client::new(client_stream);
        let session = client
            .login("test-user", "test-password")
            .await
            .map_err(|(error, _)| error)
            .unwrap();
        (session, server)
    }

    #[tokio::test]
    async fn walk_all_uids_uidmode_pages_with_partial_until_a_short_page() {
        let window = 3;
        let (mut session, server) = scripted_partial_walk_session(window).await;

        let uids = walk_all_uids_uidmode(&mut session, window, None, None)
            .await
            .expect("PARTIAL walk should complete");

        // 3 + 3 + 1 = 7 unique UIDs across the two full pages and the short one.
        assert_eq!(uids.len(), 7, "walk collects every page: {uids:?}");
        drop(session);
        let commands = server.await.expect("scripted server should finish");
        let fetches: Vec<&String> = commands
            .iter()
            .filter(|c| c.contains("UID FETCH"))
            .collect();
        assert_eq!(fetches.len(), 3, "two full pages + one short: {fetches:?}");
        assert!(
            fetches.iter().all(|c| c.contains("PARTIAL -1:-3")),
            "the partial stays newest-`window`; pagination shrinks the range: {fetches:?}"
        );
        // Page 0 spans 1:*, page 1 covers UIDs below the lowest seen (98-1=97),
        // page 2 below that (95-1=94).
        assert!(
            fetches[0].contains("UID FETCH 1:* ")
                && fetches[1].contains("UID FETCH 1:97 ")
                && fetches[2].contains("UID FETCH 1:94 "),
            "the UID range shrinks below the lowest UID of the prior page: {fetches:?}"
        );
    }

    #[tokio::test]
    async fn uidfetch_response_is_parsed_with_synthesized_uid() {
        let (mut session, server) = scripted_uidfetch_session().await;

        let fetched = timed_uid_fetch_collect(&mut session, "42", "(RFC822.SIZE)")
            .await
            .expect("UID FETCH should complete");
        let uids: Vec<Option<u32>> = fetched
            .into_iter()
            .map(|item| item.expect("fetch item").uid)
            .collect();

        assert_eq!(
            uids,
            vec![Some(42)],
            "the UIDFETCH leading number is surfaced as the message UID"
        );
        drop(session);
        server.await.expect("scripted server should finish");
    }

    /// The exact shape AOL returns for a UID-only walk page:
    /// `* <uid> UIDFETCH (UID <uid>)` with the UID present inside too.
    #[test]
    fn uidonly_uidfetch_line_parses_via_the_patched_imap_proto() {
        use async_imap::imap_proto::{self, Response};
        let line = b"* 434894 UIDFETCH (UID 434894)\r\n";
        let (rest, response) = imap_proto::parser::parse_response(line)
            .expect("patched imap-proto must parse a UID-only UIDFETCH line");
        assert!(rest.is_empty(), "the whole line is consumed: {rest:?}");
        match response {
            Response::Fetch(num, attrs) => {
                assert_eq!(num, 434894);
                assert!(
                    attrs
                        .iter()
                        .any(|a| matches!(a, imap_proto::AttributeValue::Uid(434894))),
                    "UID attribute present: {attrs:?}"
                );
            }
            other => panic!("expected Fetch, got {other:?}"),
        }
    }

    /// The linchpin shape for full-mailbox ranking: in UID Mode the rank-header
    /// fetch returns `* <uid> UIDFETCH (UID <uid> BODY[HEADER.FIELDS (…)] {N}…)`.
    /// If the patched parser choked on a UIDFETCH carrying a body-section
    /// literal, every header would come back null — no List-Id, so no quirk
    /// heal and no subscription candidates, silently.
    #[test]
    fn uidfetch_with_a_body_section_literal_parses_via_the_patched_imap_proto() {
        use async_imap::imap_proto::{self, AttributeValue, Response};
        let headers = "List-Id: <list.example.com>\r\nFrom: a@b.com\r\n\r\n";
        let line = format!(
            "* 434894 UIDFETCH (UID 434894 BODY[HEADER.FIELDS (List-Id From)] {{{}}}\r\n{headers})\r\n",
            headers.len()
        );
        let (rest, response) = imap_proto::parser::parse_response(line.as_bytes())
            .expect("patched imap-proto must parse a UIDFETCH with a body-section literal");
        assert!(rest.is_empty(), "the whole line is consumed: {rest:?}");
        match response {
            Response::Fetch(num, attrs) => {
                assert_eq!(num, 434894);
                let mut body = None;
                for attr in &attrs {
                    if let AttributeValue::BodySection {
                        data: Some(bytes), ..
                    } = attr
                    {
                        body = Some(String::from_utf8_lossy(bytes).into_owned());
                    }
                }
                let body = body.expect("the HEADER.FIELDS body section is present");
                assert!(
                    body.contains("List-Id: <list.example.com>"),
                    "the List-Id header survives the UIDFETCH parse: {body:?}"
                );
            }
            other => panic!("expected Fetch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enable_with_no_enabled_echo_still_succeeds_on_ok() {
        // A server may enable nothing (unknown extension) yet still reply OK.
        let (mut session, server) = scripted_enable_session("").await;

        let enabled = enable(&mut session, "UIDONLY")
            .await
            .expect("a tagged OK with no ENABLED line is still success");

        assert!(enabled.is_empty());
        drop(session);
        server.await.expect("scripted server should finish");
    }
}
