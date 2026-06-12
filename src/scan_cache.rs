//! In-memory cache for the per-mailbox header scans behind the `rank_*` tools.
//!
//! Ranking senders / mailing lists across a whole account re-downloads the
//! FROM/List-* headers of every message on every call. This cache validates a
//! mailbox cheaply with a single STATUS (UIDVALIDITY, UIDNEXT, MESSAGES) and
//! reuses previously-parsed rows when nothing changed, or fetches only the
//! newly-arrived messages when the tail grew.
//!
//! Correctness does not depend on invalidation hooks: every read re-validates
//! against the live STATUS triple, so a delete (which lowers MESSAGES or shifts
//! the UID/MESSAGE deltas apart) forces a full rescan on its own. Hooks merely
//! free known-stale rows sooner.

use hashbrown::{HashMap, HashSet};

use crate::imap_client::ListHeaderRow;

/// The live STATUS triple for a mailbox.
#[derive(Debug, Clone, Copy)]
pub struct MailboxStatus {
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub exists: u32,
}

/// The STATUS triple captured when a mailbox's rows were last scanned.
#[derive(Debug, Clone, Copy)]
pub struct CacheMeta {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub exists: u32,
}

/// What to do for a mailbox given its cached metadata and current status.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheDecision {
    /// Nothing changed — reuse the cached rows verbatim.
    Hit,
    /// Only new messages were appended — fetch UIDs `from_uid..` and append.
    Incremental { from_uid: u32 },
    /// UIDVALIDITY changed, messages were removed, or the deltas disagree
    /// (a mix of arrivals and deletions) — re-scan the whole mailbox.
    FullRescan,
}

impl CacheDecision {
    /// Decide how to refresh a mailbox's cached scan. Pure and total.
    pub fn from_status(cached: Option<&CacheMeta>, status: &MailboxStatus) -> Self {
        let Some(cached) = cached else {
            return Self::FullRescan;
        };
        // Without both identifiers we can't reason about the tail safely.
        let (Some(uid_validity), Some(uid_next)) = (status.uid_validity, status.uid_next) else {
            return Self::FullRescan;
        };
        // Epoch changed → every cached UID is meaningless.
        if uid_validity != cached.uid_validity {
            return Self::FullRescan;
        }
        // UIDNEXT can only advance; a regression means a rebuild.
        if uid_next < cached.uid_next || status.exists < cached.exists {
            return Self::FullRescan;
        }
        let uid_delta = uid_next - cached.uid_next;
        let msg_delta = status.exists - cached.exists;
        match (uid_delta, msg_delta) {
            (0, 0) => Self::Hit,
            // Equal deltas ⇒ pure appends (no deletions in the cached range).
            (u, m) if u == m => Self::Incremental {
                from_uid: cached.uid_next,
            },
            // Otherwise some messages were also removed — counts can't be trusted.
            _ => Self::FullRescan,
        }
    }
}

/// One cached scan: the metadata it was captured at, plus the parsed rows.
pub struct CachedScan<T> {
    pub meta: CacheMeta,
    pub rows: Vec<T>,
}

/// One sender-scan row as produced by `fetch_sender_dates`. `message_id` is
/// the logical-message identifier used to deduplicate across folders.
#[derive(Debug, Clone)]
pub struct SenderRow {
    pub email: String,
    pub display_name: String,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub message_id: Option<String>,
}

/// Per-account, per-mailbox caches for the two header-scan shapes. List-row
/// scans back both `rank_unsubscribe` and `rank_list_id` (same fetch).
#[derive(Default)]
pub struct ScanCache {
    pub sender: HashMap<(String, String), CachedScan<SenderRow>>,
    pub list: HashMap<(String, String), CachedScan<ListHeaderRow>>,
}

impl ScanCache {
    /// Drop every cached scan for an account (e.g. after a delete or move).
    pub fn invalidate_account(&mut self, account: &str) {
        self.sender.retain(|(acct, _), _| acct != account);
        self.list.retain(|(acct, _), _| acct != account);
    }
}

/// Whether a scanned row should be counted, given the Message-IDs already
/// counted this scan. The same logical message appears in every Gmail label
/// (and All Mail) with a different per-mailbox UID; deduping by Message-ID
/// counts it once. Rows without a Message-ID can't be deduped and are always
/// counted (`true`).
pub fn first_seen(seen: &mut HashSet<String>, message_id: Option<&str>) -> bool {
    match message_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => seen.insert(id.to_string()),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(uid_validity: u32, uid_next: u32, exists: u32) -> CacheMeta {
        CacheMeta {
            uid_validity,
            uid_next,
            exists,
        }
    }

    fn status(uid_validity: Option<u32>, uid_next: Option<u32>, exists: u32) -> MailboxStatus {
        MailboxStatus {
            uid_validity,
            uid_next,
            exists,
        }
    }

    #[test]
    fn no_cache_is_full_rescan() {
        let s = status(Some(1), Some(100), 99);
        assert_eq!(
            CacheDecision::from_status(None, &s),
            CacheDecision::FullRescan
        );
    }

    #[test]
    fn identical_status_is_hit() {
        let c = meta(1, 100, 99);
        let s = status(Some(1), Some(100), 99);
        assert_eq!(CacheDecision::from_status(Some(&c), &s), CacheDecision::Hit);
    }

    #[test]
    fn equal_deltas_are_incremental_from_cached_uid_next() {
        let c = meta(1, 100, 99);
        // 5 new messages, UIDNEXT advanced by 5.
        let s = status(Some(1), Some(105), 104);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::Incremental { from_uid: 100 }
        );
    }

    #[test]
    fn uidvalidity_change_is_full_rescan() {
        let c = meta(1, 100, 99);
        let s = status(Some(2), Some(100), 99);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::FullRescan
        );
    }

    #[test]
    fn message_count_decrease_is_full_rescan() {
        let c = meta(1, 100, 99);
        let s = status(Some(1), Some(100), 90);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::FullRescan
        );
    }

    #[test]
    fn arrivals_plus_deletions_disagree_and_full_rescan() {
        let c = meta(1, 100, 99);
        // 10 new UIDs but only +3 net messages ⇒ 7 were deleted.
        let s = status(Some(1), Some(110), 102);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::FullRescan
        );
    }

    #[test]
    fn missing_identifiers_are_full_rescan() {
        let c = meta(1, 100, 99);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &status(None, Some(105), 104)),
            CacheDecision::FullRescan
        );
        assert_eq!(
            CacheDecision::from_status(Some(&c), &status(Some(1), None, 104)),
            CacheDecision::FullRescan
        );
    }

    #[test]
    fn first_seen_dedups_by_message_id() {
        let mut seen = HashSet::new();
        // First sighting counts; the same id in another folder does not.
        assert!(first_seen(&mut seen, Some("<a@x>")));
        assert!(!first_seen(&mut seen, Some("<a@x>")));
        assert!(!first_seen(&mut seen, Some("  <a@x>  "))); // trimmed-equal
        // A different message counts.
        assert!(first_seen(&mut seen, Some("<b@x>")));
        // No Message-ID can't be deduped — always counted.
        assert!(first_seen(&mut seen, None));
        assert!(first_seen(&mut seen, None));
        assert!(first_seen(&mut seen, Some("   "))); // empty → treated as no id
    }

    #[test]
    fn invalidate_account_keeps_other_accounts() {
        let mut cache = ScanCache::default();
        cache.sender.insert(
            ("a".to_string(), "INBOX".to_string()),
            CachedScan {
                meta: meta(1, 10, 9),
                rows: vec![],
            },
        );
        cache.sender.insert(
            ("b".to_string(), "INBOX".to_string()),
            CachedScan {
                meta: meta(1, 10, 9),
                rows: vec![],
            },
        );
        cache.invalidate_account("a");
        assert!(
            !cache
                .sender
                .contains_key(&("a".to_string(), "INBOX".to_string()))
        );
        assert!(
            cache
                .sender
                .contains_key(&("b".to_string(), "INBOX".to_string()))
        );
    }
}
