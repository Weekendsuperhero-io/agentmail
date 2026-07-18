//! Bounded, process-local cache of mailbox layout metadata.
//!
//! The catalog deliberately cannot hold message UIDs, counts, headers, or
//! other message-derived data. It exists only to avoid a network `LIST` on
//! every mailbox completion and special-use mailbox lookup.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hashbrown::HashMap;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::Result;
use crate::imap_client::MailboxLayout;

const MAILBOX_CATALOG_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_MAILBOXES_PER_ACCOUNT: usize = 4_096;
const MAX_LAYOUT_BYTES_PER_ACCOUNT: usize = 1024 * 1024;

#[derive(Default)]
struct AccountState {
    generation: u64,
    snapshot: Option<CatalogSnapshot>,
}

struct CatalogSnapshot {
    entries: Arc<[MailboxLayout]>,
    loaded_at: Instant,
}

/// Layout snapshots and per-account refresh locks.
///
/// Refresh locks are kept for the process lifetime. Their key set is bounded
/// by configured accounts because callers validate the account before access.
#[derive(Default)]
pub(crate) struct MailboxCatalog {
    states: Mutex<HashMap<String, AccountState>>,
    refresh_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl MailboxCatalog {
    pub(crate) fn get(&self, account: &str) -> Option<Arc<[MailboxLayout]>> {
        let started = Instant::now();
        let entries = self.get_at(account, started);
        if let Some(ref entries) = entries {
            trace_catalog("hit", started, 0, entries.len(), true);
        }
        entries
    }

    /// Return a fresh snapshot or run one refresh for the account.
    ///
    /// Production loaders that use a pooled IMAP session acquire that session
    /// before calling this method. All refresh paths use the same session-then-
    /// gate ordering, including callers that already own a session.
    pub(crate) async fn get_or_refresh<F, Fut>(
        &self,
        account: &str,
        load: F,
    ) -> Result<Arc<[MailboxLayout]>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<MailboxLayout>>>,
    {
        let started = Instant::now();
        if let Some(entries) = self.get_at(account, started) {
            trace_catalog("hit", started, 0, entries.len(), true);
            return Ok(entries);
        }

        let refresh_lock = self.refresh_lock(account);
        let _refresh_guard = refresh_lock.lock().await;

        if let Some(entries) = self.get_at(account, Instant::now()) {
            trace_catalog("shared_hit", started, 0, entries.len(), true);
            return Ok(entries);
        }

        let generation = self.generation(account);
        let entries = match load().await {
            Ok(entries) => entries,
            Err(error) => {
                trace_catalog("refresh_error", started, 1, 0, false);
                return Err(error);
            }
        };
        let (entries, retained) =
            self.store_if_generation(account, generation, entries, Instant::now());
        let cache_status = if retained { "miss" } else { "not_retained" };
        trace_catalog(cache_status, started, 1, entries.len(), retained);
        Ok(entries)
    }

    pub(crate) fn invalidate(&self, account: &str) {
        let mut states = self.states.lock();
        let state = states.entry(account.to_string()).or_default();
        state.generation = state.generation.wrapping_add(1);
        state.snapshot = None;
    }

    fn get_at(&self, account: &str, now: Instant) -> Option<Arc<[MailboxLayout]>> {
        let mut states = self.states.lock();
        let state = states.get_mut(account)?;
        let is_fresh = state.snapshot.as_ref().is_some_and(|snapshot| {
            now.saturating_duration_since(snapshot.loaded_at) < MAILBOX_CATALOG_TTL
        });
        if !is_fresh {
            state.snapshot = None;
            return None;
        }
        state
            .snapshot
            .as_ref()
            .map(|snapshot| Arc::clone(&snapshot.entries))
    }

    fn generation(&self, account: &str) -> u64 {
        self.states
            .lock()
            .entry(account.to_string())
            .or_default()
            .generation
    }

    fn refresh_lock(&self, account: &str) -> Arc<AsyncMutex<()>> {
        Arc::clone(
            self.refresh_locks
                .lock()
                .entry(account.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    fn store_if_generation(
        &self,
        account: &str,
        generation: u64,
        entries: Vec<MailboxLayout>,
        loaded_at: Instant,
    ) -> (Arc<[MailboxLayout]>, bool) {
        let cacheable = is_cacheable(&entries);
        let entries: Arc<[MailboxLayout]> = entries.into();
        let mut states = self.states.lock();
        let state = states.entry(account.to_string()).or_default();
        let retained = cacheable && state.generation == generation;
        if retained {
            state.snapshot = Some(CatalogSnapshot {
                entries: Arc::clone(&entries),
                loaded_at,
            });
        } else if state.generation == generation {
            state.snapshot = None;
        }
        (entries, retained)
    }
}

fn is_cacheable(entries: &[MailboxLayout]) -> bool {
    if entries.len() > MAX_MAILBOXES_PER_ACCOUNT {
        return false;
    }
    entries
        .iter()
        .try_fold(0usize, |total, entry| {
            let mut bytes = total.checked_add(entry.path.len())?;
            bytes = bytes.checked_add(entry.delimiter.as_deref().map_or(0, str::len))?;
            entry
                .roles
                .iter()
                .try_fold(bytes, |subtotal, role| subtotal.checked_add(role.len()))
        })
        .is_some_and(|layout_bytes| layout_bytes <= MAX_LAYOUT_BYTES_PER_ACCOUNT)
}

/// Resolve a selectable RFC 6154 Trash mailbox, with conservative name
/// fallbacks for servers that omit the special-use attribute.
pub(crate) fn resolve_trash(entries: &[MailboxLayout]) -> Option<String> {
    resolve_selectable_special_mailbox(
        entries,
        "trash",
        &[
            "Trash",
            "[Gmail]/Trash",
            "INBOX.Trash",
            "Deleted Messages",
            "Deleted",
        ],
        &["trash", "deleted"],
    )
}

/// Resolve a selectable RFC 6154 Drafts mailbox, with conservative name
/// fallbacks for servers that omit the special-use attribute.
pub(crate) fn resolve_drafts(entries: &[MailboxLayout]) -> Option<String> {
    resolve_selectable_special_mailbox(
        entries,
        "drafts",
        &["Drafts", "[Gmail]/Drafts", "INBOX.Drafts"],
        &["draft"],
    )
}

fn resolve_selectable_special_mailbox(
    entries: &[MailboxLayout],
    role: &str,
    exact_names: &[&str],
    name_fragments: &[&str],
) -> Option<String> {
    let selectable = || entries.iter().filter(|entry| entry.is_selectable());

    if let Some(entry) = selectable().find(|entry| entry.has_role(role)) {
        return Some(entry.path.clone());
    }

    for candidate in exact_names {
        if let Some(entry) = selectable().find(|entry| entry.path.eq_ignore_ascii_case(candidate)) {
            return Some(entry.path.clone());
        }
    }

    selectable()
        .find(|entry| {
            let lower_name = entry.path.to_lowercase();
            name_fragments
                .iter()
                .any(|fragment| lower_name.contains(fragment))
        })
        .map(|entry| entry.path.clone())
}

fn trace_catalog(
    cache: &'static str,
    started: Instant,
    imap_commands: u8,
    result_count: usize,
    retained: bool,
) {
    tracing::debug!(
        target: "agentmail",
        operation = "mailbox_catalog",
        cache,
        elapsed_ms = started.elapsed().as_millis(),
        imap_commands,
        result_count,
        retained,
        "mailbox layout catalog access"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::AgentmailError;

    fn entry(path: impl Into<String>) -> MailboxLayout {
        MailboxLayout {
            path: path.into(),
            delimiter: Some("/".to_string()),
            no_select: false,
            no_inferiors: false,
            roles: Vec::new(),
        }
    }

    fn special_entry(path: &str, role: &str, no_select: bool) -> MailboxLayout {
        let mut entry = entry(path);
        entry.no_select = no_select;
        entry.roles.push(role.to_string());
        entry
    }

    #[test]
    fn trash_resolver_ignores_unselectable_role_candidate() {
        let entries = [
            special_entry("Trash container", "trash", true),
            entry("Deleted Messages"),
        ];

        assert_eq!(resolve_trash(&entries).as_deref(), Some("Deleted Messages"));
    }

    #[test]
    fn drafts_resolver_ignores_unselectable_named_candidate() {
        let mut container = entry("Drafts");
        container.no_select = true;
        let entries = [container, entry("INBOX.Drafts")];

        assert_eq!(resolve_drafts(&entries).as_deref(), Some("INBOX.Drafts"));
    }

    #[test]
    fn special_use_resolver_preserves_selectable_child_of_unselectable_parent() {
        let mut parent = entry("Archive");
        parent.no_select = true;
        let entries = [parent, special_entry("Archive/Trash", "trash", false)];

        assert_eq!(resolve_trash(&entries).as_deref(), Some("Archive/Trash"));
    }

    #[test]
    fn trash_resolver_returns_none_for_only_unselectable_candidates() {
        let entries = [special_entry("Trash", "trash", true)];

        assert!(resolve_trash(&entries).is_none());
    }

    #[test]
    fn drafts_resolver_returns_none_for_only_unselectable_candidates() {
        let entries = [special_entry("Drafts", "drafts", true)];

        assert!(resolve_drafts(&entries).is_none());
    }

    #[test]
    fn fresh_snapshot_is_returned() {
        let catalog = MailboxCatalog::default();
        let now = Instant::now();
        let generation = catalog.generation("work");
        catalog.store_if_generation("work", generation, vec![entry("INBOX")], now);

        let result = catalog.get_at("work", now + MAILBOX_CATALOG_TTL - Duration::from_nanos(1));

        assert_eq!(result.as_deref().map(|entries| entries.len()), Some(1));
    }

    #[test]
    fn snapshot_expires_at_ttl_boundary() {
        let catalog = MailboxCatalog::default();
        let now = Instant::now();
        let generation = catalog.generation("work");
        catalog.store_if_generation("work", generation, vec![entry("INBOX")], now);

        let result = catalog.get_at("work", now + MAILBOX_CATALOG_TTL);

        assert!(result.is_none());
    }

    #[test]
    fn invalidation_is_scoped_to_one_account() {
        let catalog = MailboxCatalog::default();
        let now = Instant::now();
        for account in ["work", "personal"] {
            let generation = catalog.generation(account);
            catalog.store_if_generation(account, generation, vec![entry("INBOX")], now);
        }

        catalog.invalidate("work");

        assert!(catalog.get_at("work", now).is_none());
        assert!(catalog.get_at("personal", now).is_some());
    }

    #[test]
    fn maximum_mailbox_count_is_cacheable() {
        let entries: Vec<_> = (0..MAX_MAILBOXES_PER_ACCOUNT)
            .map(|index| entry(format!("Mailbox {index}")))
            .collect();

        assert!(is_cacheable(&entries));
    }

    #[test]
    fn mailbox_count_over_limit_is_not_cacheable() {
        let entries: Vec<_> = (0..=MAX_MAILBOXES_PER_ACCOUNT)
            .map(|index| entry(format!("Mailbox {index}")))
            .collect();

        assert!(!is_cacheable(&entries));
    }

    #[test]
    fn maximum_layout_bytes_are_cacheable() {
        let mut entry = entry("x".repeat(MAX_LAYOUT_BYTES_PER_ACCOUNT - 1));
        entry.delimiter = Some("/".to_string());
        let entries = [entry];

        assert!(is_cacheable(&entries));
    }

    #[test]
    fn layout_bytes_over_limit_are_not_cacheable() {
        let entries = [entry("x".repeat(MAX_LAYOUT_BYTES_PER_ACCOUNT + 1))];

        assert!(!is_cacheable(&entries));
    }

    #[test]
    fn oversized_result_is_returned_but_not_retained() {
        let catalog = MailboxCatalog::default();
        let generation = catalog.generation("work");
        let oversized = vec![entry("x".repeat(MAX_LAYOUT_BYTES_PER_ACCOUNT + 1))];

        let (entries, retained) =
            catalog.store_if_generation("work", generation, oversized, Instant::now());

        assert_eq!(entries.len(), 1);
        assert!(!retained);
        assert!(catalog.get("work").is_none());
    }

    #[tokio::test]
    async fn empty_successful_listing_is_cached() {
        let catalog = MailboxCatalog::default();
        let calls = AtomicUsize::new(0);

        catalog
            .get_or_refresh("work", || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
            .await
            .expect("first empty listing should load");
        catalog
            .get_or_refresh("work", || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![entry("unexpected")])
            })
            .await
            .expect("cached empty listing should be returned");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_refresh_does_not_serve_expired_snapshot() {
        let catalog = MailboxCatalog::default();
        let generation = catalog.generation("work");
        let expired_at = Instant::now() - MAILBOX_CATALOG_TTL;
        catalog.store_if_generation("work", generation, vec![entry("stale")], expired_at);

        let result = catalog
            .get_or_refresh("work", || async {
                Err(AgentmailError::Other("test refresh failure".to_string()))
            })
            .await;

        assert!(result.is_err());
        assert!(catalog.get("work").is_none());
    }

    #[tokio::test]
    async fn concurrent_refreshes_run_loader_once() {
        let catalog = Arc::new(MailboxCatalog::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let loader_started = Arc::new(Notify::new());
        let release_loader = Arc::new(Notify::new());

        let first_catalog = Arc::clone(&catalog);
        let first_calls = Arc::clone(&calls);
        let first_started = Arc::clone(&loader_started);
        let first_release = Arc::clone(&release_loader);
        let first = tokio::spawn(async move {
            first_catalog
                .get_or_refresh("work", || async move {
                    first_calls.fetch_add(1, Ordering::SeqCst);
                    first_started.notify_one();
                    first_release.notified().await;
                    Ok(vec![entry("INBOX")])
                })
                .await
        });

        loader_started.notified().await;
        let second_catalog = Arc::clone(&catalog);
        let second_calls = Arc::clone(&calls);
        let second = tokio::spawn(async move {
            second_catalog
                .get_or_refresh("work", || async move {
                    second_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![entry("should not load")])
                })
                .await
        });

        release_loader.notify_one();
        let first_result = first.await.expect("first refresh task should complete");
        let second_result = second.await.expect("second refresh task should complete");

        assert!(first_result.is_ok());
        assert!(second_result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalidation_during_refresh_prevents_retention() {
        let catalog = MailboxCatalog::default();
        let generation = catalog.generation("work");
        catalog.invalidate("work");

        let (entries, retained) =
            catalog.store_if_generation("work", generation, vec![entry("stale")], Instant::now());

        assert_eq!(entries.len(), 1);
        assert!(!retained);
        assert!(catalog.get("work").is_none());
    }
}
