//! Legacy header-scan cache types retained for Rust API compatibility.
//!
//! [`crate::Agentmail`] now uses a private persistent cache with stronger
//! stable-snapshot validation. These public legacy types remain so the 0.2
//! Rust API does not break downstream callers.

use hashbrown::{HashMap, HashSet};

use crate::imap_client::ListHeaderRow;

/// The live STATUS (or SELECT-derived) metadata for a mailbox.
#[derive(Debug, Clone, Copy)]
pub struct MailboxStatus {
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub exists: u32,
    /// Present when the server returned HIGHESTMODSEQ (CONDSTORE).
    pub highest_modseq: Option<u64>,
}

/// Metadata captured when a mailbox's rows were last scanned.
#[derive(Debug, Clone, Copy)]
pub struct CacheMeta {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub exists: u32,
    /// Set when the scan was captured under CONDSTORE; `None` otherwise.
    pub highest_modseq: Option<u64>,
}

impl CacheMeta {
    /// Placeholder meta for a cache entry that will be filled after SELECT.
    pub fn placeholder() -> Self {
        Self {
            uid_validity: 0,
            uid_next: 0,
            exists: 0,
            highest_modseq: None,
        }
    }
}

/// What to do for a mailbox given its cached metadata and current status.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheDecision {
    /// Nothing changed — reuse the cached rows verbatim.
    Hit,
    /// Only new messages were appended — fetch UIDs `from_uid..` and append.
    Incremental { from_uid: u32 },
    /// UIDVALIDITY stable but the mailbox is not a pure append (deletes and/or
    /// mixed arrivals). Reconcile by live UID set: prune vanished, fetch missing.
    MembershipRefresh,
    /// Cold cache, UIDVALIDITY change, UIDNEXT regression, or missing identifiers
    /// — re-scan the whole mailbox.
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
        if uid_next < cached.uid_next {
            return Self::FullRescan;
        }

        // exists can drop (deletes) — that is MembershipRefresh, not FullRescan.
        if status.exists < cached.exists {
            return Self::MembershipRefresh;
        }
        if uid_next == cached.uid_next && status.exists == cached.exists {
            Self::Hit
        } else {
            // STATUS values alone do not form an atomic snapshot. AgentMail's
            // production cache proves a tail append with UID SEARCH bracketed
            // by two identical EXAMINE snapshots before using it incrementally.
            Self::MembershipRefresh
        }
    }
}

/// Given cached row UIDs and the live mailbox UID set, return the sorted list
/// of live UIDs that are not yet in the cache (need a header fetch).
pub fn missing_uids(cached_uids: &HashSet<u32>, live_uids: &HashSet<u32>) -> Vec<u32> {
    let mut missing: Vec<u32> = live_uids
        .iter()
        .copied()
        .filter(|u| !cached_uids.contains(u))
        .collect();
    missing.sort_unstable();
    missing
}

/// One cached scan: the metadata it was captured at, plus the parsed rows.
///
/// `scanned_uids` is the set of mailbox UIDs whose headers were already
/// examined for this cache shape. For sender scans this is nearly the row
/// set; for list scans it also includes non-matching (non-bulk) messages so
/// MembershipRefresh does not re-fetch them every time.
pub struct CachedScan<T> {
    pub meta: CacheMeta,
    pub rows: Vec<T>,
    pub scanned_uids: HashSet<u32>,
}

/// One sender-scan row as produced by `fetch_sender_dates`. `message_id` is
/// the logical-message identifier used to deduplicate across folders.
#[derive(Debug, Clone)]
pub struct SenderRow {
    /// IMAP UID within the scanned mailbox (for membership prune/refresh).
    pub uid: u32,
    pub email: String,
    pub display_name: String,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub message_id: Option<String>,
}

/// Per-account, per-mailbox caches for the two header-scan shapes. List-row
/// scans back both `top_subscriptions` and `top_mailing_lists` (same fetch).
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
            highest_modseq: None,
        }
    }

    fn meta_ms(uid_validity: u32, uid_next: u32, exists: u32, ms: u64) -> CacheMeta {
        CacheMeta {
            uid_validity,
            uid_next,
            exists,
            highest_modseq: Some(ms),
        }
    }

    fn status(uid_validity: Option<u32>, uid_next: Option<u32>, exists: u32) -> MailboxStatus {
        MailboxStatus {
            uid_validity,
            uid_next,
            exists,
            highest_modseq: None,
        }
    }

    fn status_ms(
        uid_validity: Option<u32>,
        uid_next: Option<u32>,
        exists: u32,
        ms: u64,
    ) -> MailboxStatus {
        MailboxStatus {
            uid_validity,
            uid_next,
            exists,
            highest_modseq: Some(ms),
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
    fn equal_deltas_require_stable_membership_evidence() {
        let c = meta(1, 100, 99);
        let s = status(Some(1), Some(105), 104);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::MembershipRefresh
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
    fn message_count_decrease_is_membership_refresh() {
        let c = meta(1, 100, 99);
        let s = status(Some(1), Some(100), 90);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::MembershipRefresh
        );
    }

    #[test]
    fn arrivals_plus_deletions_are_membership_refresh() {
        let c = meta(1, 100, 99);
        // 10 new UIDs but only +3 net messages ⇒ 7 were deleted.
        let s = status(Some(1), Some(110), 102);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::MembershipRefresh
        );
    }

    #[test]
    fn uid_next_regression_is_full_rescan() {
        let c = meta(1, 100, 99);
        let s = status(Some(1), Some(90), 89);
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
    fn equal_modseq_with_count_change_refreshes_membership() {
        let c = meta_ms(1, 100, 99, 50);
        let s = status_ms(Some(1), Some(100), 90, 50);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::MembershipRefresh
        );
    }

    #[test]
    fn advanced_modseq_with_stable_counts_is_hit_via_triple() {
        // Flag-only change: modseq advanced, STATUS triple unchanged → Hit
        // for rank rows (they do not store flags).
        let c = meta_ms(1, 100, 99, 50);
        let s = status_ms(Some(1), Some(100), 99, 60);
        assert_eq!(CacheDecision::from_status(Some(&c), &s), CacheDecision::Hit);
    }

    #[test]
    fn advanced_modseq_with_exists_drop_is_membership_refresh() {
        let c = meta_ms(1, 100, 99, 50);
        let s = status_ms(Some(1), Some(100), 90, 60);
        assert_eq!(
            CacheDecision::from_status(Some(&c), &s),
            CacheDecision::MembershipRefresh
        );
    }

    #[test]
    fn modseq_only_on_one_side_falls_back_to_status_rules() {
        let c = meta_ms(1, 100, 99, 50);
        // Status has no modseq — use STATUS triple (Hit).
        let s = status(Some(1), Some(100), 99);
        assert_eq!(CacheDecision::from_status(Some(&c), &s), CacheDecision::Hit);

        let c2 = meta(1, 100, 99);
        let s2 = status_ms(Some(1), Some(110), 102, 99);
        assert_eq!(
            CacheDecision::from_status(Some(&c2), &s2),
            CacheDecision::MembershipRefresh
        );
    }

    #[test]
    fn missing_uids_returns_sorted_live_not_in_cache() {
        let cached: HashSet<u32> = [1, 2, 3].into_iter().collect();
        let live: HashSet<u32> = [2, 3, 5, 4].into_iter().collect();
        assert_eq!(missing_uids(&cached, &live), vec![4, 5]);
    }

    #[test]
    fn missing_uids_empty_when_cache_covers_live() {
        let cached: HashSet<u32> = [1, 2, 3].into_iter().collect();
        let live: HashSet<u32> = [1, 2].into_iter().collect();
        assert!(missing_uids(&cached, &live).is_empty());
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
                scanned_uids: HashSet::new(),
            },
        );
        cache.sender.insert(
            ("b".to_string(), "INBOX".to_string()),
            CachedScan {
                meta: meta(1, 10, 9),
                rows: vec![],
                scanned_uids: HashSet::new(),
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
