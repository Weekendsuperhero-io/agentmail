//! Pure mailbox-selection policy for account-wide scans.
//!
//! Discovery and mutation deliberately use different plans. Read-only
//! discovery can use one aggregate `\All` mailbox, while mutations enumerate
//! storage mailboxes so a virtual view is never used as the write target.

use crate::imap_client::MailboxLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanPurpose {
    Discovery,
    Mutation,
}

impl ScanPurpose {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Mutation => "mutation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanStrategy {
    AllMailbox,
    Enumerated,
}

impl ScanStrategy {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AllMailbox => "all_mailbox",
            Self::Enumerated => "enumerated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanPlan {
    pub(crate) mailboxes: Vec<String>,
    pub(crate) strategy: ScanStrategy,
}

/// Plan an account-wide scan from one mailbox-layout snapshot.
///
/// A selectable server-declared `\All` mailbox is authoritative for discovery.
/// A conservative name fallback supports older servers that do not advertise
/// SPECIAL-USE. Mutations always enumerate non-virtual storage mailboxes.
pub(crate) fn plan_account_scan(entries: &[MailboxLayout], purpose: ScanPurpose) -> ScanPlan {
    if purpose == ScanPurpose::Discovery
        && let Some(all_mailbox) = preferred_all_mailbox(entries)
    {
        return ScanPlan {
            mailboxes: vec![all_mailbox],
            strategy: ScanStrategy::AllMailbox,
        };
    }

    let mut mailboxes: Vec<String> = entries
        .iter()
        .filter(|entry| is_enumerated_scan_target(entry))
        .map(|entry| entry.path.clone())
        .collect();
    mailboxes.sort_by(|left, right| {
        left.to_ascii_lowercase()
            .cmp(&right.to_ascii_lowercase())
            .then_with(|| left.cmp(right))
    });
    mailboxes.dedup();

    ScanPlan {
        mailboxes,
        strategy: ScanStrategy::Enumerated,
    }
}

fn preferred_all_mailbox(entries: &[MailboxLayout]) -> Option<String> {
    let mut declared: Vec<&str> = entries
        .iter()
        .filter(|entry| !entry.no_select && entry.has_role("all"))
        .map(|entry| entry.path.as_str())
        .collect();
    sort_names(&mut declared);
    if let Some(name) = declared.first() {
        return Some((*name).to_string());
    }

    let mut inferred: Vec<&str> = entries
        .iter()
        .filter(|entry| {
            !entry.no_select
                && entry.roles.is_empty()
                && inferred_category(&entry.path, entry.delimiter.as_deref())
                    == Some(InferredCategory::All)
        })
        .map(|entry| entry.path.as_str())
        .collect();
    sort_names(&mut inferred);
    inferred.first().map(|name| (*name).to_string())
}

fn sort_names(names: &mut [&str]) {
    names.sort_by(|left, right| {
        left.to_ascii_lowercase()
            .cmp(&right.to_ascii_lowercase())
            .then_with(|| left.cmp(right))
    });
}

fn is_enumerated_scan_target(entry: &MailboxLayout) -> bool {
    if entry.no_select {
        return false;
    }

    // These categories either contain unwanted messages or commonly present
    // virtual duplicates of messages stored elsewhere.
    const SKIP_ROLES: &[&str] = &["all", "drafts", "flagged", "important", "junk", "trash"];
    if entry
        .roles
        .iter()
        .any(|role| SKIP_ROLES.contains(&role.as_str()))
    {
        return false;
    }

    // Trust declared roles. Only use English name inference when the server
    // supplied no recognized special-use role for this mailbox.
    !entry.roles.is_empty() || inferred_category(&entry.path, entry.delimiter.as_deref()).is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InferredCategory {
    All,
    Drafts,
    Flagged,
    Junk,
    Trash,
}

fn inferred_category(path: &str, delimiter: Option<&str>) -> Option<InferredCategory> {
    let lower = path.trim().to_ascii_lowercase();
    let leaf = match delimiter.filter(|delimiter| !delimiter.is_empty()) {
        Some(delimiter) => lower.rsplit(delimiter).next().unwrap_or(lower.as_str()),
        None => lower.rsplit(['/', '.']).next().unwrap_or(lower.as_str()),
    }
    .trim();

    match leaf {
        "all mail" | "all messages" => Some(InferredCategory::All),
        "draft" | "drafts" => Some(InferredCategory::Drafts),
        "flagged" | "starred" | "important" => Some(InferredCategory::Flagged),
        // "bulk" is AOL/Yahoo's spam folder name; it carries no \Junk
        // special-use attribute there, so the name is the only signal.
        "junk" | "junk email" | "spam" | "bulk" | "bulk mail" => Some(InferredCategory::Junk),
        "bin" | "deleted" | "deleted items" | "deleted messages" | "trash" => {
            Some(InferredCategory::Trash)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, roles: &[&str]) -> MailboxLayout {
        MailboxLayout {
            path: path.to_string(),
            delimiter: Some("/".to_string()),
            no_select: false,
            no_inferiors: false,
            roles: roles.iter().map(|role| (*role).to_string()).collect(),
        }
    }

    #[test]
    fn discovery_uses_only_declared_all_mailbox() {
        let entries = [
            entry("INBOX", &[]),
            entry("Everything", &["all"]),
            entry("Archive", &["archive"]),
        ];

        let plan = plan_account_scan(&entries, ScanPurpose::Discovery);

        assert_eq!(plan.strategy, ScanStrategy::AllMailbox);
        assert_eq!(plan.mailboxes, ["Everything"]);
    }

    #[test]
    fn declared_all_wins_over_name_fallback() {
        let entries = [
            entry("[Gmail]/All Mail", &[]),
            entry("Everything", &["all"]),
        ];

        let plan = plan_account_scan(&entries, ScanPurpose::Discovery);

        assert_eq!(plan.mailboxes, ["Everything"]);
    }

    #[test]
    fn discovery_infers_all_mail_for_older_servers() {
        let entries = [entry("INBOX", &[]), entry("[Gmail]/All Mail", &[])];

        let plan = plan_account_scan(&entries, ScanPurpose::Discovery);

        assert_eq!(plan.strategy, ScanStrategy::AllMailbox);
        assert_eq!(plan.mailboxes, ["[Gmail]/All Mail"]);
    }

    #[test]
    fn no_select_all_is_never_scanned() {
        let mut aggregate = entry("Everything", &["all"]);
        aggregate.no_select = true;
        let entries = [entry("INBOX", &[]), aggregate];

        let plan = plan_account_scan(&entries, ScanPurpose::Discovery);

        assert_eq!(plan.strategy, ScanStrategy::Enumerated);
        assert_eq!(plan.mailboxes, ["INBOX"]);
    }

    #[test]
    fn mutation_never_uses_aggregate_or_virtual_views() {
        let entries = [
            entry("INBOX", &[]),
            entry("Everything", &["all"]),
            entry("Starred", &["flagged"]),
            entry("Priority", &["important"]),
            entry("Drafts", &["drafts"]),
            entry("Spam", &["junk"]),
            entry("Bin", &["trash"]),
            entry("Mixed", &["archive", "junk"]),
        ];

        let plan = plan_account_scan(&entries, ScanPurpose::Mutation);

        assert_eq!(plan.strategy, ScanStrategy::Enumerated);
        assert_eq!(plan.mailboxes, ["INBOX"]);
    }

    #[test]
    fn storage_special_use_mailboxes_are_enumerated() {
        let entries = [
            entry("Sent", &["sent"]),
            entry("Archive", &["archive"]),
            entry("Memos", &["memos"]),
            entry("Scheduled", &["scheduled"]),
            entry("Snoozed", &["snoozed"]),
        ];

        let plan = plan_account_scan(&entries, ScanPurpose::Mutation);

        assert_eq!(
            plan.mailboxes,
            ["Archive", "Memos", "Scheduled", "Sent", "Snoozed"]
        );
    }

    #[test]
    fn aol_bulk_spam_folder_is_excluded_by_name() {
        // AOL/Yahoo name their spam folder "Bulk" and (on AOL) advertise no
        // \Junk role for it, so the name heuristic is the only guard.
        let entries = [
            entry("INBOX", &[]),
            entry("Bulk", &[]),
            entry("Archive", &["archive"]),
        ];

        let plan = plan_account_scan(&entries, ScanPurpose::Discovery);

        assert_eq!(plan.mailboxes, ["Archive", "INBOX"]);
    }

    #[test]
    fn exact_name_fallback_avoids_broad_false_positives() {
        let entries = [
            entry("All Hands", &[]),
            entry("Draft Plans", &[]),
            entry("Important Documents", &[]),
            entry("Project.Trash", &[]),
            entry("Project/Trash", &[]),
        ];

        let plan = plan_account_scan(&entries, ScanPurpose::Mutation);

        assert_eq!(
            plan.mailboxes,
            [
                "All Hands",
                "Draft Plans",
                "Important Documents",
                "Project.Trash"
            ]
        );
    }

    #[test]
    fn name_fallback_uses_the_advertised_hierarchy_delimiter() {
        let mut trash = entry("INBOX.Trash", &[]);
        trash.delimiter = Some(".".to_string());

        let plan = plan_account_scan(&[entry("INBOX", &[]), trash], ScanPurpose::Mutation);

        assert_eq!(plan.mailboxes, ["INBOX"]);
    }

    #[test]
    fn authoritative_storage_role_overrides_a_suspicious_name() {
        let entries = [entry("Trash", &["archive"])];

        let plan = plan_account_scan(&entries, ScanPurpose::Mutation);

        assert_eq!(plan.mailboxes, ["Trash"]);
    }

    #[test]
    fn plans_are_stable_and_exact_names_are_deduplicated() {
        let entries = [entry("zeta", &[]), entry("INBOX", &[]), entry("INBOX", &[])];

        let plan = plan_account_scan(&entries, ScanPurpose::Mutation);

        assert_eq!(plan.mailboxes, ["INBOX", "zeta"]);
    }
}
