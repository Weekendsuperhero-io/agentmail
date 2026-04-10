use std::sync::Arc;
use std::time::Duration;

use async_imap::Session;
use futures::StreamExt;
use tokio::net::TcpStream;
use tokio_native_tls::TlsStream;
use tracing::debug;

use crate::AgentmailError;
use crate::config::AccountConfig;
use crate::error::Result;
use crate::parser;
use crate::types::*;

/// The concrete IMAP session type used throughout.
pub type ImapSession = Session<TlsStream<TcpStream>>;

/// Callback for reporting progress: `(completed, total)`.
pub type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Type alias for raw fetch items: `(uid, size, flags, body_bytes)`.
type RawFetchItems = Vec<(u32, Option<u32>, Vec<String>, Vec<u8>)>;

/// Default timeout for IMAP operations (connect, login, fetch, etc.).
const IMAP_TIMEOUT: Duration = Duration::from_secs(90);

/// Shorter timeout for keep-alive pings.
const PING_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Timeout helpers
// ---------------------------------------------------------------------------

/// Wrap a future with the standard IMAP timeout.
async fn imap_timeout<F, T, E>(future: F) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(IMAP_TIMEOUT, future).await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(e)) => Err(AgentmailError::Other(e.to_string())),
        Err(_elapsed) => Err(AgentmailError::Other(format!(
            "IMAP operation timed out after {}s",
            IMAP_TIMEOUT.as_secs()
        ))),
    }
}

/// UID FETCH + stream collect with timeout.
async fn timed_uid_fetch_collect(
    session: &mut ImapSession,
    uid_set: &str,
    items: &str,
) -> Result<Vec<std::result::Result<async_imap::types::Fetch, async_imap::error::Error>>> {
    imap_timeout(async {
        let stream = session.uid_fetch(uid_set, items).await?;
        Ok::<_, async_imap::error::Error>(stream.collect::<Vec<_>>().await)
    })
    .await
}

/// Select a mailbox with timeout. Use this instead of calling `session.select()` directly.
pub async fn select(
    session: &mut ImapSession,
    mailbox: &str,
) -> Result<async_imap::types::Mailbox> {
    imap_timeout(session.select(mailbox)).await
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Connect to an IMAP server over TLS and authenticate.
pub async fn connect(config: &AccountConfig, password: &str) -> Result<ImapSession> {
    let addr = format!("{}:{}", config.host, config.port);
    let tcp = imap_timeout(TcpStream::connect(&addr)).await?;

    let connector = native_tls::TlsConnector::new()
        .map_err(|e| AgentmailError::Other(format!("TLS connector error: {}", e)))?;
    let connector = tokio_native_tls::TlsConnector::from(connector);
    let tls = imap_timeout(connector.connect(&config.host, tcp)).await?;

    let client = async_imap::Client::new(tls);
    let login_fut = client.login(&config.username, password);
    let session = match tokio::time::timeout(IMAP_TIMEOUT, login_fut).await {
        Ok(Ok(session)) => session,
        Ok(Err((err, _client))) => return Err(AgentmailError::Imap(err)),
        Err(_elapsed) => {
            return Err(AgentmailError::Other(format!(
                "IMAP login timed out after {}s",
                IMAP_TIMEOUT.as_secs()
            )));
        }
    };
    Ok(session)
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
    let caps = imap_timeout(session.capabilities()).await?;
    let mut result: Vec<String> = caps
        .iter()
        .map(|c| match c {
            async_imap::types::Capability::Imap4rev1 => "IMAP4rev1".to_string(),
            async_imap::types::Capability::Auth(s) => format!("AUTH={}", s),
            async_imap::types::Capability::Atom(s) => s.clone(),
        })
        .collect();
    result.sort();
    Ok(result)
}

// ---------------------------------------------------------------------------
// Mailbox operations
// ---------------------------------------------------------------------------

/// List all mailboxes for an account. Uses LIST + STATUS per mailbox.
/// Map an RFC 6154 special-use `NameAttribute` to a role string.
fn role_from_attributes(attrs: &[async_imap::types::NameAttribute<'_>]) -> Option<String> {
    use async_imap::types::NameAttribute;
    for attr in attrs {
        let role = match attr {
            NameAttribute::All => "all",
            NameAttribute::Archive => "archive",
            NameAttribute::Drafts => "drafts",
            NameAttribute::Flagged => "flagged",
            NameAttribute::Junk => "junk",
            NameAttribute::Sent => "sent",
            NameAttribute::Trash => "trash",
            _ => continue,
        };
        return Some(role.to_string());
    }
    None
}

pub async fn list_mailboxes(
    session: &mut ImapSession,
    account_name: &str,
) -> Result<Vec<MailboxInfo>> {
    use async_imap::types::NameAttribute;

    let names: Vec<_> = imap_timeout(async {
        let stream = session.list(Some(""), Some("*")).await?;
        Ok::<_, async_imap::error::Error>(stream.collect::<Vec<_>>().await)
    })
    .await?;

    let mut result = Vec::with_capacity(names.len());
    for item in names {
        let name_ref = item.map_err(AgentmailError::Imap)?;
        let name = name_ref.name().to_string();
        let delimiter = name_ref.delimiter().map(|c| c.to_string());
        let attrs = name_ref.attributes();

        let no_select = attrs.contains(&NameAttribute::NoSelect);
        let no_inferiors = attrs.contains(&NameAttribute::NoInferiors);
        let role = role_from_attributes(attrs);

        // NoSelect mailboxes can't be SELECTed — skip the STATUS call.
        let (total, unseen, recent) = if no_select {
            (0, 0, 0)
        } else {
            let status = imap_timeout(session.status(&name, "(MESSAGES UNSEEN RECENT)")).await?;
            (status.exists, status.unseen.unwrap_or(0), status.recent)
        };

        result.push(MailboxInfo {
            name: name.clone(),
            account: account_name.to_string(),
            total_messages: total,
            unseen_messages: unseen,
            recent_messages: recent,
            delimiter,
            path: name,
            no_select,
            no_inferiors,
            role,
        });
    }
    Ok(result)
}

/// Lightweight mailbox entry: name + key attributes (no STATUS calls).
pub struct MailboxEntry {
    pub name: String,
    pub no_select: bool,
    pub role: Option<String>,
}

/// List all mailboxes with key attributes (without STATUS calls — much faster
/// than `list_mailboxes`).
pub async fn list_mailbox_entries(session: &mut ImapSession) -> Result<Vec<MailboxEntry>> {
    use async_imap::types::NameAttribute;

    let names: Vec<_> = imap_timeout(async {
        let stream = session.list(Some(""), Some("*")).await?;
        Ok::<_, async_imap::error::Error>(stream.collect::<Vec<_>>().await)
    })
    .await?;

    let mut result = Vec::with_capacity(names.len());
    for item in names {
        let name_ref = item.map_err(AgentmailError::Imap)?;
        let attrs = name_ref.attributes();
        result.push(MailboxEntry {
            name: name_ref.name().to_string(),
            no_select: attrs.contains(&NameAttribute::NoSelect),
            role: role_from_attributes(attrs),
        });
    }
    Ok(result)
}

/// List all selectable mailbox names (without STATUS calls).
pub async fn list_mailbox_names(session: &mut ImapSession) -> Result<Vec<String>> {
    let entries = list_mailbox_entries(session).await?;
    Ok(entries
        .into_iter()
        .filter(|e| !e.no_select)
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
) -> Result<(Vec<MessageInfo>, u32)> {
    let mb = imap_timeout(session.select(mailbox)).await?;
    let total = mb.exists;
    debug!(mailbox, account = account_name, total, "SELECT complete");

    if total == 0 {
        debug!("Mailbox is empty, returning early");
        return Ok((Vec::new(), 0));
    }

    // Get all UIDs, sort descending (newest first)
    let uids_raw = imap_timeout(session.uid_search("ALL")).await?;
    let mut uids: Vec<u32> = uids_raw.into_iter().collect();
    debug!(uid_count = uids.len(), "UID SEARCH ALL returned");
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
        return Ok((Vec::new(), total));
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
    debug!(fetched = messages.len(), "Messages parsed");
    Ok((messages, total))
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
) -> Result<(Vec<MessageInfo>, u32)> {
    imap_timeout(session.select(mailbox)).await?;

    let query = build_search_query(criteria);
    let uids_raw = imap_timeout(session.uid_search(&query)).await?;
    let mut uids: Vec<u32> = uids_raw.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    let total_matches = uids.len() as u32;

    let start = offset.min(uids.len());
    let end = (start + limit).min(uids.len());
    let page_uids = &uids[start..end];

    if page_uids.is_empty() {
        return Ok((Vec::new(), total_matches));
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
    Ok((messages, total_matches))
}

/// Build an IMAP SEARCH query string from SearchCriteria (public wrapper).
pub fn build_search_query_pub(criteria: &SearchCriteria) -> String {
    build_search_query(criteria)
}

/// Run a UID SEARCH with a raw query string. Returns matching UIDs.
/// Caller must have already selected the mailbox.
pub async fn search_uids(session: &mut ImapSession, query: &str) -> Result<Vec<u32>> {
    let uids = imap_timeout(session.uid_search(query)).await?;
    Ok(uids.into_iter().collect())
}

/// Fetch only FROM and DATE headers for all messages in a mailbox.
/// Uses BODY.PEEK to avoid setting \Seen.
pub async fn fetch_sender_dates(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
) -> Result<Vec<(String, String, Option<chrono::DateTime<chrono::Utc>>)>> {
    let mb = imap_timeout(session.select(mailbox)).await?;

    if mb.exists == 0 {
        return Ok(Vec::new());
    }

    let uids_raw = imap_timeout(session.uid_search("ALL")).await?;
    let uids: Vec<u32> = uids_raw.into_iter().collect();
    let total = uids.len() as u64;

    debug!(uid_count = uids.len(), "fetch_sender_dates: UIDs collected");

    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let mut results = Vec::with_capacity(uids.len());
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetched = timed_uid_fetch_collect(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (FROM DATE)])",
        )
        .await?;

        debug!(
            chunk_size = chunk.len(),
            stream_items = fetched.len(),
            "fetch_sender_dates: batch collected"
        );

        for item in fetched {
            let fetch = item.map_err(AgentmailError::Imap)?;
            let header_bytes = fetch.header().unwrap_or(&[]);

            match parser::parse_sender_date(header_bytes) {
                Ok(tuple) => results.push(tuple),
                Err(e) => {
                    debug!(
                        uid = ?fetch.uid,
                        error = %e,
                        "fetch_sender_dates: skipping unparseable message"
                    );
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
    let (email, name, _date) = parser::parse_sender_date(header_bytes)?;
    Ok((email, name))
}

/// Fetch the parsed sender (email, display_name) for a batch of UIDs.
/// Returns Vec of (uid, email, display_name). Skips unparseable messages.
pub async fn fetch_senders_batch(
    session: &mut ImapSession,
    uids: &[u32],
) -> Result<Vec<(u32, String, String)>> {
    let mut results = Vec::new();
    for chunk in uids.chunks(1000) {
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
            if let Ok((email, name, _)) = parser::parse_sender_date(header_bytes) {
                results.push((uid, email, name));
            }
        }
    }
    Ok(results)
}

/// A row from `fetch_list_headers` — one per message that has List-Unsubscribe or List-Id.
pub struct ListHeaderRow {
    pub uid: u32,
    pub list_unsubscribe: Option<String>,
    pub list_unsubscribe_post: Option<String>,
    pub list_id: Option<String>,
    pub sender_email: String,
    pub sender_name: String,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
}

/// Fetch List-Unsubscribe, List-Unsubscribe-Post, List-Id, FROM, and DATE headers.
/// Only includes messages that have at least one of List-Unsubscribe or
/// List-Unsubscribe-Post, indicating bulk/marketing mail.
pub async fn fetch_list_headers(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
) -> Result<Vec<ListHeaderRow>> {
    let mb = imap_timeout(session.select(mailbox)).await?;

    if mb.exists == 0 {
        return Ok(Vec::new());
    }

    let uids_raw = imap_timeout(session.uid_search("ALL")).await?;
    let uids: Vec<u32> = uids_raw.into_iter().collect();
    let total = uids.len() as u64;

    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetched = timed_uid_fetch_collect(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (List-Unsubscribe List-Unsubscribe-Post List-Id FROM DATE)])",
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

            let list_unsub = extract_header_value(&header_str, "List-Unsubscribe");
            let list_id = extract_header_value(&header_str, "List-Id");

            let list_unsub_post = extract_header_value(&header_str, "List-Unsubscribe-Post");

            // Require at least one of List-Unsubscribe or List-Unsubscribe-Post
            if list_unsub.is_none() && list_unsub_post.is_none() {
                continue;
            }

            let (sender_email, sender_name, date) =
                parser::parse_sender_date(header_bytes).unwrap_or_default();

            results.push(ListHeaderRow {
                uid,
                list_unsubscribe: list_unsub,
                list_unsubscribe_post: list_unsub_post,
                list_id,
                sender_email,
                sender_name,
                date,
            });
        }

        completed += chunk.len() as u64;
        if let Some(progress) = on_progress {
            progress(completed, total);
        }
    }

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
        "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[])"
    } else {
        "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])"
    };

    debug!(uid_set = %uid_set, fetch_items, "UID FETCH request");

    let fetched = timed_uid_fetch_collect(session, &uid_set, fetch_items).await?;

    debug!(stream_items = fetched.len(), "UID FETCH stream collected");

    // Extract owned data from the IMAP fetch results so we can parse off-thread
    let mut raw_items: RawFetchItems = Vec::with_capacity(fetched.len());
    for item in fetched {
        match &item {
            Ok(f) => debug!(
                uid = f.uid,
                has_body = f.body().is_some(),
                has_header = f.header().is_some(),
                size = f.size,
                "FETCH item"
            ),
            Err(e) => debug!(error = %e, "FETCH item error"),
        }
        let fetch = item.map_err(AgentmailError::Imap)?;
        let uid = fetch.uid.unwrap_or(0);
        let size = fetch.size;
        let flags: Vec<String> = fetch.flags().map(|f| flag_to_string(&f)).collect();
        let raw = if include_content {
            fetch.body().unwrap_or(&[])
        } else {
            fetch.header().unwrap_or(&[])
        };
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
pub async fn sync(session: &mut ImapSession) -> Result<()> {
    imap_timeout(session.noop()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Attachment detection via Content-Type header
// ---------------------------------------------------------------------------

/// Fetch UIDs of messages that have attachments.
/// Uses lightweight Content-Type header check: multipart/mixed indicates attachments.
/// Returns UIDs sorted newest-first.
pub async fn fetch_attachment_uids(
    session: &mut ImapSession,
    mailbox: &str,
    on_progress: Option<&ProgressFn>,
) -> Result<Vec<u32>> {
    let mb = imap_timeout(session.select(mailbox)).await?;
    if mb.exists == 0 {
        return Ok(Vec::new());
    }

    let uids_raw = imap_timeout(session.uid_search("ALL")).await?;
    let uids: Vec<u32> = uids_raw.into_iter().collect();
    let total = uids.len() as u64;
    let mut attachment_uids: Vec<u32> = Vec::new();
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetched = timed_uid_fetch_collect(
            session,
            &uid_set,
            "(UID BODY.PEEK[HEADER.FIELDS (Content-Type)])",
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
                attachment_uids.push(uid);
            }
        }

        completed += chunk.len() as u64;
        if let Some(progress) = on_progress {
            progress(completed, total);
        }
    }

    // Sort newest-first (highest UID first)
    attachment_uids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(attachment_uids)
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

/// Delete messages by UID, processing in chunks.
/// If `trash_mailbox` is set, attempts MOVE first; falls back to flag+expunge on failure.
pub async fn bulk_delete_messages(
    session: &mut ImapSession,
    uids: &[u32],
    trash_mailbox: Option<&str>,
    on_progress: Option<&ProgressFn>,
) -> Result<BulkDeleteResult> {
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut trash_fallback = false;
    let mut use_trash = trash_mailbox;
    let total = uids.len() as u64;

    for chunk in uids.chunks(500) {
        let uid_set: String = chunk
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let result: std::result::Result<(), AgentmailError> = if let Some(trash) = use_trash {
            let move_result = imap_timeout(session.uid_mv(&uid_set, trash))
                .await
                .map(|_| ());
            if move_result.is_err() {
                // Trash MOVE failed — fall back to flag+expunge for all remaining
                trash_fallback = true;
                use_trash = None;
                flag_and_expunge(session, &uid_set).await
            } else {
                move_result
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

/// Flag messages as deleted and expunge them (permanent delete).
async fn flag_and_expunge(
    session: &mut ImapSession,
    uid_set: &str,
) -> std::result::Result<(), AgentmailError> {
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

/// Move a message to another mailbox by UID.
pub async fn move_message(session: &mut ImapSession, uid: u32, destination: &str) -> Result<()> {
    let uid_str = uid.to_string();
    imap_timeout(session.uid_mv(&uid_str, destination)).await?;
    Ok(())
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
    imap_timeout(session.append(drafts_mailbox, Some("\\Draft"), None, rfc822_message)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw source
// ---------------------------------------------------------------------------

/// Fetch the raw RFC822 source of a single message.
pub async fn get_message_source(
    session: &mut ImapSession,
    mailbox: &str,
    uid: u32,
) -> Result<Vec<u8>> {
    imap_timeout(session.select(mailbox)).await?;
    let uid_str = uid.to_string();
    let fetched = timed_uid_fetch_collect(session, &uid_str, "BODY.PEEK[]").await?;

    let fetch = fetched
        .into_iter()
        .next()
        .ok_or(AgentmailError::MessageNotFound(uid))?
        .map_err(AgentmailError::Imap)?;
    let body = fetch.body().ok_or(AgentmailError::MessageNotFound(uid))?;
    Ok(body.to_vec())
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

pub async fn fetch_unsubscribe_headers(
    session: &mut ImapSession,
    mailbox: &str,
    uid: u32,
) -> Result<UnsubscribeHeaders> {
    imap_timeout(session.select(mailbox)).await?;
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
    let query = format!(
        "HEADER \"{}\" \"{}\"",
        escape_imap_string(header_name),
        escape_imap_string(header_value)
    );
    let uids = imap_timeout(session.uid_search(&query)).await?;
    Ok(uids.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build an IMAP SEARCH query string from SearchCriteria.
fn build_search_query(criteria: &SearchCriteria) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(ref text) = criteria.text {
        parts.push(format!("TEXT \"{}\"", escape_imap_string(text)));
    }
    if let Some(ref from) = criteria.from {
        parts.push(format!("FROM \"{}\"", escape_imap_string(from)));
    }
    if let Some(ref subject) = criteria.subject {
        parts.push(format!("SUBJECT \"{}\"", escape_imap_string(subject)));
    }
    if let Some(ref to) = criteria.to {
        parts.push(format!("TO \"{}\"", escape_imap_string(to)));
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
        parts.push(format!(
            "HEADER \"{}\" \"{}\"",
            escape_imap_string(key),
            escape_imap_string(value)
        ));
    }

    if parts.is_empty() {
        "ALL".to_string()
    } else {
        parts.join(" ")
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
) -> Result<FlagScanResult> {
    let mb = imap_timeout(session.select(mailbox)).await?;
    if mb.exists == 0 {
        return Ok(FlagScanResult {
            flags: Vec::new(),
            colors: Vec::new(),
        });
    }

    let uids_raw = imap_timeout(session.uid_search("ALL")).await?;
    let uids: Vec<u32> = uids_raw.into_iter().collect();
    let total = uids.len() as u64;
    let mut flag_counts: hashbrown::HashMap<String, u32> = hashbrown::HashMap::new();
    let mut color_counts: hashbrown::HashMap<String, u32> = hashbrown::HashMap::new();
    let mut completed = 0u64;

    for chunk in uids.chunks(1000) {
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
                *flag_counts.entry(name.clone()).or_insert(0) += 1;
            }
            // Resolve Apple Mail color from MailFlagBit combinations
            if let Some(color) = crate::bits_to_color(&msg_flags) {
                *color_counts.entry(color.to_string()).or_insert(0) += 1;
            }
        }

        completed += chunk.len() as u64;
        if let Some(progress) = on_progress {
            progress(completed, total);
        }
    }

    let mut flags: Vec<(String, u32)> = flag_counts.into_iter().collect();
    flags.sort_by(|a, b| b.1.cmp(&a.1));
    let mut colors: Vec<(String, u32)> = color_counts.into_iter().collect();
    colors.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(FlagScanResult { flags, colors })
}

/// Escape a string for use in IMAP quoted strings.
fn escape_imap_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
pub async fn timed_uid_fetch_collect_pub(
    session: &mut ImapSession,
    uid_set: &str,
    query: &str,
) -> Result<Vec<std::result::Result<async_imap::types::Fetch, async_imap::error::Error>>> {
    timed_uid_fetch_collect(session, uid_set, query).await
}

/// Public wrapper for `extract_header_value`.
pub fn extract_header_value_pub(headers: &str, name: &str) -> Option<String> {
    extract_header_value(headers, name)
}

/// Extract a header value from raw header text by name.
fn extract_header_value(headers: &str, name: &str) -> Option<String> {
    let lower_name = name.to_lowercase();
    for line in headers.lines() {
        let lower_line = line.to_lowercase();
        if lower_line.starts_with(&format!("{}:", lower_name)) {
            let value = line[name.len() + 1..].trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}
