//! Durable forward-recovery journal for non-native IMAP MOVE.
//!
//! The header cache is disposable; mutation intent is not. This database is
//! therefore separate, uses FULL synchronous durability, and keeps one active
//! claim per source UID until cleanup is verified or an operator resolves an
//! ambiguous copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{AgentmailError, Result};

const JOURNAL_SCHEMA_VERSION: i64 = 1;
const TERMINAL_HISTORY_DAYS: i64 = 30;
const MAX_TERMINAL_HISTORY: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveJournalState {
    Prepared,
    CopyInFlight,
    Copied,
    DeleteInFlight,
    Complete,
    CopyFailed,
    NeedsAttention,
}

impl MoveJournalState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::CopyInFlight => "copy_in_flight",
            Self::Copied => "copied",
            Self::DeleteInFlight => "delete_in_flight",
            Self::Complete => "complete",
            Self::CopyFailed => "copy_failed",
            Self::NeedsAttention => "needs_attention",
        }
    }

    fn parse(value: &str) -> std::result::Result<Self, rusqlite::Error> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "copy_in_flight" => Ok(Self::CopyInFlight),
            "copied" => Ok(Self::Copied),
            "delete_in_flight" => Ok(Self::DeleteInFlight),
            "complete" => Ok(Self::Complete),
            "copy_failed" => Ok(Self::CopyFailed),
            "needs_attention" => Ok(Self::NeedsAttention),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown move journal state {other:?}").into(),
            )),
        }
    }

    fn is_active(self) -> bool {
        matches!(
            self,
            Self::Prepared
                | Self::CopyInFlight
                | Self::Copied
                | Self::DeleteInFlight
                | Self::NeedsAttention
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MoveOperation {
    pub(crate) operation_id: String,
    pub(crate) account_key: String,
    pub(crate) source_mailbox: String,
    pub(crate) source_uid_validity: u32,
    pub(crate) source_uid: u32,
    pub(crate) destination: String,
    pub(crate) destination_uid_validity: u32,
    pub(crate) destination_uid_next: u32,
    pub(crate) state: MoveJournalState,
    pub(crate) copied_uid_validity: Option<u32>,
    pub(crate) copied_uid: Option<u32>,
    pub(crate) detail: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct PrepareMove<'a> {
    pub(crate) account_key: &'a str,
    pub(crate) source_mailbox: &'a str,
    pub(crate) source_uid_validity: u32,
    pub(crate) source_uid: u32,
    pub(crate) destination: &'a str,
    pub(crate) destination_uid_validity: u32,
    pub(crate) destination_uid_next: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct MutationJournal {
    path: Option<Arc<PathBuf>>,
}

impl MutationJournal {
    pub(crate) const FILE_NAME: &'static str = "mutation-journal.sqlite3";

    pub(crate) fn at_path(path: PathBuf) -> Self {
        Self {
            path: Some(Arc::new(path)),
        }
    }

    pub(crate) fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("AGENTMAIL_CACHE_DIR") {
            return Some(PathBuf::from(path).join(Self::FILE_NAME));
        }
        let root = dirs::cache_dir()?;
        Some(root.join("agentmail").join(Self::FILE_NAME))
    }

    pub(crate) fn default_persistent() -> Self {
        Self {
            path: Self::default_path().map(Arc::new),
        }
    }

    pub(crate) fn is_persistent(&self) -> bool {
        self.path.is_some()
    }

    pub(crate) async fn prepare(&self, request: PrepareMove<'_>) -> Result<MoveOperation> {
        let path = self.required_path()?;
        let request = OwnedPrepareMove::from(request);
        run_db(path, move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            prune_terminal_history(&transaction)?;
            if let Some(existing) = find_active_source(
                &transaction,
                &request.account_key,
                &request.source_mailbox,
                request.source_uid_validity,
                request.source_uid,
            )? {
                if existing.destination == request.destination {
                    transaction.commit()?;
                    return Ok(existing);
                }
                return Err(AgentmailError::Other(format!(
                    "source UID {} already belongs to pending move {} targeting '{}'",
                    request.source_uid, existing.operation_id, existing.destination
                )));
            }

            let operation_id = uuid::Uuid::new_v4().to_string();
            let now = Utc::now();
            transaction.execute(
                "INSERT INTO move_operations (
                    operation_id, account_key, source_mailbox,
                    source_uid_validity, source_uid, destination,
                    destination_uid_validity, destination_uid_next, state,
                    created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'prepared', ?9, ?9)",
                params![
                    operation_id,
                    request.account_key,
                    request.source_mailbox,
                    i64::from(request.source_uid_validity),
                    i64::from(request.source_uid),
                    request.destination,
                    i64::from(request.destination_uid_validity),
                    i64::from(request.destination_uid_next),
                    now.to_rfc3339(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO active_move_sources (
                    account_key, source_mailbox, source_uid_validity,
                    source_uid, operation_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    request.account_key,
                    request.source_mailbox,
                    i64::from(request.source_uid_validity),
                    i64::from(request.source_uid),
                    operation_id,
                ],
            )?;
            let operation = get_operation(&transaction, &operation_id)?.ok_or_else(|| {
                AgentmailError::Other("new move journal entry disappeared".to_string())
            })?;
            transaction.commit()?;
            Ok(operation)
        })
        .await
    }

    pub(crate) async fn transition(
        &self,
        operation_id: &str,
        expected: &[MoveJournalState],
        next: MoveJournalState,
        detail: Option<&str>,
    ) -> Result<MoveOperation> {
        let path = self.required_path()?;
        let operation_id = operation_id.to_string();
        let expected = expected.to_vec();
        let detail = detail.map(str::to_string);
        run_db(path, move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = get_operation(&transaction, &operation_id)?.ok_or_else(|| {
                AgentmailError::Other(format!("unknown move operation '{operation_id}'"))
            })?;
            if !expected.contains(&current.state) {
                return Err(AgentmailError::Other(format!(
                    "move operation '{operation_id}' is {}, expected one of {}",
                    current.state.as_str(),
                    expected
                        .iter()
                        .map(|state| state.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            update_state(&transaction, &operation_id, next, detail.as_deref())?;
            if !next.is_active() {
                transaction.execute(
                    "DELETE FROM active_move_sources WHERE operation_id = ?1",
                    params![operation_id],
                )?;
            }
            let operation = get_operation(&transaction, &operation_id)?.ok_or_else(|| {
                AgentmailError::Other("move journal entry disappeared".to_string())
            })?;
            transaction.commit()?;
            Ok(operation)
        })
        .await
    }

    pub(crate) async fn record_copied(
        &self,
        operation_id: &str,
        copied_uid_validity: Option<u32>,
        copied_uid: Option<u32>,
    ) -> Result<MoveOperation> {
        let path = self.required_path()?;
        let operation_id = operation_id.to_string();
        run_db(path, move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = get_operation(&transaction, &operation_id)?.ok_or_else(|| {
                AgentmailError::Other(format!("unknown move operation '{operation_id}'"))
            })?;
            if current.state != MoveJournalState::CopyInFlight {
                return Err(AgentmailError::Other(format!(
                    "move operation '{operation_id}' is {}, expected copy_in_flight",
                    current.state.as_str()
                )));
            }
            transaction.execute(
                "UPDATE move_operations
                    SET state = 'copied', copied_uid_validity = ?2,
                        copied_uid = ?3, detail = NULL, updated_at = ?4
                  WHERE operation_id = ?1",
                params![
                    operation_id,
                    copied_uid_validity.map(i64::from),
                    copied_uid.map(i64::from),
                    Utc::now().to_rfc3339(),
                ],
            )?;
            let operation = get_operation(&transaction, &operation_id)?.ok_or_else(|| {
                AgentmailError::Other("move journal entry disappeared".to_string())
            })?;
            transaction.commit()?;
            Ok(operation)
        })
        .await
    }

    pub(crate) async fn get(&self, operation_id: &str) -> Result<Option<MoveOperation>> {
        let path = self.required_path()?;
        let operation_id = operation_id.to_string();
        run_db(path, move |connection| {
            get_operation(connection, &operation_id)
        })
        .await
    }

    pub(crate) async fn list_pending(&self, account_key: &str) -> Result<Vec<MoveOperation>> {
        let path = self.required_path()?;
        let account_key = account_key.to_string();
        run_db(path, move |connection| {
            let mut statement = connection.prepare(
                "SELECT operation_id, account_key, source_mailbox,
                        source_uid_validity, source_uid, destination,
                        destination_uid_validity, destination_uid_next, state,
                        copied_uid_validity, copied_uid, detail, created_at, updated_at
                   FROM move_operations
                  WHERE account_key = ?1
                    AND state IN ('prepared', 'copy_in_flight', 'copied',
                                  'delete_in_flight', 'needs_attention')
                  ORDER BY created_at, operation_id",
            )?;
            let rows = statement.query_map(params![account_key], operation_from_row)?;
            let mut operations = Vec::new();
            for row in rows {
                operations.push(row?);
            }
            Ok(operations)
        })
        .await
    }

    fn required_path(&self) -> Result<Arc<PathBuf>> {
        self.path.clone().ok_or_else(|| {
            AgentmailError::Other(
                "durable mutation journal is unavailable; refusing COPY-based MOVE".to_string(),
            )
        })
    }
}

#[derive(Debug, Clone)]
struct OwnedPrepareMove {
    account_key: String,
    source_mailbox: String,
    source_uid_validity: u32,
    source_uid: u32,
    destination: String,
    destination_uid_validity: u32,
    destination_uid_next: u32,
}

impl From<PrepareMove<'_>> for OwnedPrepareMove {
    fn from(value: PrepareMove<'_>) -> Self {
        Self {
            account_key: value.account_key.to_string(),
            // RFC 3501 makes only INBOX case-insensitive. Other mailbox names
            // may legitimately differ by case and must remain distinct journal
            // identities, destinations, and active-source claims.
            source_mailbox: canonical_inbox(value.source_mailbox),
            source_uid_validity: value.source_uid_validity,
            source_uid: value.source_uid,
            destination: canonical_inbox(value.destination),
            destination_uid_validity: value.destination_uid_validity,
            destination_uid_next: value.destination_uid_next,
        }
    }
}

fn canonical_inbox(mailbox: &str) -> String {
    if mailbox.eq_ignore_ascii_case("INBOX") {
        "INBOX".to_string()
    } else {
        mailbox.to_string()
    }
}

async fn run_db<T: Send + 'static>(
    path: Arc<PathBuf>,
    operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || {
        let mut connection = open_connection(&path)?;
        operation(&mut connection)
    })
    .await
    .map_err(|error| AgentmailError::Other(format!("mutation journal task failed: {error}")))?
}

fn open_connection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        let parent_existed = parent.exists();
        std::fs::create_dir_all(parent)?;
        // Never chmod a caller-owned broad existing directory (notably /tmp).
        // Newly created journal directories are private from first use.
        if !parent_existed {
            set_private_directory(parent)?;
        }
    }
    let connection = Connection::open(path)?;
    set_private_file(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    initialize_schema(&connection)?;
    Ok(connection)
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS journal_meta (
             schema_version INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS move_operations (
             operation_id TEXT PRIMARY KEY,
             account_key TEXT NOT NULL,
             source_mailbox TEXT NOT NULL,
             source_uid_validity INTEGER NOT NULL,
             source_uid INTEGER NOT NULL,
             destination TEXT NOT NULL,
             destination_uid_validity INTEGER NOT NULL,
             destination_uid_next INTEGER NOT NULL,
             state TEXT NOT NULL,
             copied_uid_validity INTEGER,
             copied_uid INTEGER,
             detail TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS active_move_sources (
             account_key TEXT NOT NULL,
             source_mailbox TEXT NOT NULL,
             source_uid_validity INTEGER NOT NULL,
             source_uid INTEGER NOT NULL,
             operation_id TEXT NOT NULL REFERENCES move_operations(operation_id)
                 ON DELETE CASCADE,
             PRIMARY KEY (
                 account_key, source_mailbox, source_uid_validity, source_uid
             )
         );
         CREATE INDEX IF NOT EXISTS move_operations_account_state
             ON move_operations(account_key, state, created_at);",
    )?;
    let version = connection
        .query_row(
            "SELECT schema_version FROM journal_meta LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    match version {
        None => {
            connection.execute(
                "INSERT INTO journal_meta(schema_version) VALUES (?1)",
                params![JOURNAL_SCHEMA_VERSION],
            )?;
        }
        Some(JOURNAL_SCHEMA_VERSION) => {}
        Some(other) => {
            return Err(AgentmailError::Other(format!(
                "unsupported mutation journal schema version {other}"
            )));
        }
    }
    Ok(())
}

fn find_active_source(
    connection: &Connection,
    account_key: &str,
    source_mailbox: &str,
    source_uid_validity: u32,
    source_uid: u32,
) -> Result<Option<MoveOperation>> {
    connection
        .query_row(
            "SELECT o.operation_id, o.account_key, o.source_mailbox,
                    o.source_uid_validity, o.source_uid, o.destination,
                    o.destination_uid_validity, o.destination_uid_next, o.state,
                    o.copied_uid_validity, o.copied_uid, o.detail,
                    o.created_at, o.updated_at
               FROM active_move_sources a
               JOIN move_operations o ON o.operation_id = a.operation_id
              WHERE a.account_key = ?1 AND a.source_mailbox = ?2
                AND a.source_uid_validity = ?3 AND a.source_uid = ?4",
            params![
                account_key,
                source_mailbox,
                i64::from(source_uid_validity),
                i64::from(source_uid),
            ],
            operation_from_row,
        )
        .optional()
        .map_err(AgentmailError::from)
}

fn get_operation(connection: &Connection, operation_id: &str) -> Result<Option<MoveOperation>> {
    connection
        .query_row(
            "SELECT operation_id, account_key, source_mailbox,
                    source_uid_validity, source_uid, destination,
                    destination_uid_validity, destination_uid_next, state,
                    copied_uid_validity, copied_uid, detail, created_at, updated_at
               FROM move_operations WHERE operation_id = ?1",
            params![operation_id],
            operation_from_row,
        )
        .optional()
        .map_err(AgentmailError::from)
}

fn update_state(
    connection: &Connection,
    operation_id: &str,
    state: MoveJournalState,
    detail: Option<&str>,
) -> Result<()> {
    let changed = connection.execute(
        "UPDATE move_operations
            SET state = ?2, detail = ?3, updated_at = ?4
          WHERE operation_id = ?1",
        params![
            operation_id,
            state.as_str(),
            detail,
            Utc::now().to_rfc3339(),
        ],
    )?;
    if changed != 1 {
        return Err(AgentmailError::Other(format!(
            "unknown move operation '{operation_id}'"
        )));
    }
    Ok(())
}

fn prune_terminal_history(connection: &Connection) -> Result<()> {
    let cutoff = Utc::now()
        .checked_sub_signed(chrono::Duration::days(TERMINAL_HISTORY_DAYS))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
        .to_rfc3339();
    connection.execute(
        "DELETE FROM move_operations
          WHERE state IN ('complete', 'copy_failed') AND updated_at < ?1",
        params![cutoff],
    )?;
    connection.execute(
        "DELETE FROM move_operations
          WHERE operation_id IN (
              SELECT operation_id FROM move_operations
               WHERE state IN ('complete', 'copy_failed')
               ORDER BY updated_at DESC, operation_id DESC
               LIMIT -1 OFFSET ?1
          )",
        params![i64::try_from(MAX_TERMINAL_HISTORY).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

fn operation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MoveOperation> {
    let created_at: String = row.get(12)?;
    let updated_at: String = row.get(13)?;
    Ok(MoveOperation {
        operation_id: row.get(0)?,
        account_key: row.get(1)?,
        source_mailbox: row.get(2)?,
        source_uid_validity: sql_u32(row.get(3)?, 3)?,
        source_uid: sql_u32(row.get(4)?, 4)?,
        destination: row.get(5)?,
        destination_uid_validity: sql_u32(row.get(6)?, 6)?,
        destination_uid_next: sql_u32(row.get(7)?, 7)?,
        state: MoveJournalState::parse(&row.get::<_, String>(8)?)?,
        copied_uid_validity: row
            .get::<_, Option<i64>>(9)?
            .map(|value| sql_u32(value, 9))
            .transpose()?,
        copied_uid: row
            .get::<_, Option<i64>>(10)?
            .map(|value| sql_u32(value, 10))
            .transpose()?,
        detail: row.get(11)?,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })?
            .with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(&updated_at)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    13,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })?
            .with_timezone(&Utc),
    })
}

fn sql_u32(value: i64, column: usize) -> rusqlite::Result<u32> {
    u32::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            error.into(),
        )
    })
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_journal(name: &str) -> MutationJournal {
        let directory =
            std::env::temp_dir().join(format!("agentmail-journal-{name}-{}", uuid::Uuid::new_v4()));
        let path = directory.join(MutationJournal::FILE_NAME);
        MutationJournal::at_path(path)
    }

    #[tokio::test]
    async fn same_source_same_destination_resumes_but_other_destination_conflicts() {
        let journal = test_journal("claim");
        let request = PrepareMove {
            account_key: "account",
            source_mailbox: "INBOX",
            source_uid_validity: 7,
            source_uid: 42,
            destination: "Archive",
            destination_uid_validity: 9,
            destination_uid_next: 100,
        };
        let first = journal.prepare(request.clone()).await.unwrap();
        let second = journal.prepare(request).await.unwrap();
        assert_eq!(first.operation_id, second.operation_id);

        let conflict = journal
            .prepare(PrepareMove {
                destination: "Other",
                account_key: "account",
                source_mailbox: "INBOX",
                source_uid_validity: 7,
                source_uid: 42,
                destination_uid_validity: 10,
                destination_uid_next: 1,
            })
            .await
            .unwrap_err();
        assert!(conflict.to_string().contains("already belongs"));
    }

    #[tokio::test]
    async fn terminal_transition_releases_the_source_claim() {
        let journal = test_journal("release");
        let first = journal
            .prepare(PrepareMove {
                account_key: "account",
                source_mailbox: "INBOX",
                source_uid_validity: 7,
                source_uid: 42,
                destination: "Archive",
                destination_uid_validity: 9,
                destination_uid_next: 100,
            })
            .await
            .unwrap();
        journal
            .transition(
                &first.operation_id,
                &[MoveJournalState::Prepared],
                MoveJournalState::CopyFailed,
                Some("server rejected COPY"),
            )
            .await
            .unwrap();
        let replacement = journal
            .prepare(PrepareMove {
                account_key: "account",
                source_mailbox: "INBOX",
                source_uid_validity: 7,
                source_uid: 42,
                destination: "Other",
                destination_uid_validity: 10,
                destination_uid_next: 1,
            })
            .await
            .unwrap();
        assert_ne!(first.operation_id, replacement.operation_id);
    }

    #[tokio::test]
    async fn only_inbox_is_case_insensitive_in_journal_identities() {
        let journal = test_journal("mailbox-case");
        let first = journal
            .prepare(PrepareMove {
                account_key: "account",
                source_mailbox: "inbox",
                source_uid_validity: 7,
                source_uid: 42,
                destination: "Archive",
                destination_uid_validity: 9,
                destination_uid_next: 100,
            })
            .await
            .unwrap();
        let resumed = journal
            .prepare(PrepareMove {
                account_key: "account",
                source_mailbox: "INBOX",
                source_uid_validity: 7,
                source_uid: 42,
                destination: "Archive",
                destination_uid_validity: 9,
                destination_uid_next: 100,
            })
            .await
            .unwrap();
        assert_eq!(first.operation_id, resumed.operation_id);

        let conflict = journal
            .prepare(PrepareMove {
                account_key: "account",
                source_mailbox: "INBOX",
                source_uid_validity: 7,
                source_uid: 42,
                destination: "archive",
                destination_uid_validity: 9,
                destination_uid_next: 100,
            })
            .await
            .unwrap_err();
        assert!(conflict.to_string().contains("already belongs"));
    }

    #[tokio::test]
    async fn preparing_a_move_prunes_only_expired_terminal_history() {
        let journal = test_journal("history-prune");
        let finished = journal
            .prepare(PrepareMove {
                account_key: "account",
                source_mailbox: "INBOX",
                source_uid_validity: 7,
                source_uid: 42,
                destination: "Archive",
                destination_uid_validity: 9,
                destination_uid_next: 100,
            })
            .await
            .unwrap();
        journal
            .transition(
                &finished.operation_id,
                &[MoveJournalState::Prepared],
                MoveJournalState::Complete,
                None,
            )
            .await
            .unwrap();

        let path = journal.required_path().unwrap();
        let operation_id = finished.operation_id.clone();
        run_db(path, move |connection| {
            let old = (Utc::now() - chrono::Duration::days(TERMINAL_HISTORY_DAYS + 1)).to_rfc3339();
            connection.execute(
                "UPDATE move_operations SET updated_at = ?2 WHERE operation_id = ?1",
                params![operation_id, old],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        journal
            .prepare(PrepareMove {
                account_key: "account",
                source_mailbox: "INBOX",
                source_uid_validity: 7,
                source_uid: 43,
                destination: "Archive",
                destination_uid_validity: 9,
                destination_uid_next: 101,
            })
            .await
            .unwrap();
        assert!(journal.get(&finished.operation_id).await.unwrap().is_none());
    }
}
