//! Task management (SEP-1686): background execution of long-running tools.

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use hashbrown::HashMap;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ListTasksResult, Task, TaskStatus};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Task results are retained for 24 hours from task creation.
pub(super) const TASK_TTL_MS: u64 = 86_400_000;
/// Maximum number of unexpired tasks and enqueue reservations per process.
pub(super) const MAX_LIVE_TASKS: usize = 128;
/// Protocol page size for `tasks/list`.
pub(super) const TASK_PAGE_SIZE: usize = 25;

const TASK_POLL_INTERVAL_MS: u64 = 2_000;
const TASK_CURSOR_PREFIX: &str = "am-task-v1";

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Tools whose `annotations.destructive_hint` is `true`.
/// Destructive tasks targeting the same account are serialized — each waits for
/// the previous destructive task to finish before starting.
pub(super) const DESTRUCTIVE_TOOLS: &[&str] = &[
    "delete_messages",
    "delete_by_sender",
    "delete_list_id",
    "unsubscribe_message",
];

/// Try to extract the `account` field from a tool call's JSON arguments.
pub(super) fn extract_account(
    args: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    args.as_ref()?.get("account")?.as_str().map(String::from)
}

pub(super) struct ManagedTask {
    pub(super) meta: Task,
    pub(super) result: Arc<parking_lot::Mutex<Option<Result<CallToolResult, McpError>>>>,
    pub(super) cancel: CancellationToken,
    pub(super) handle: JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct TaskRecord {
    created_at_millis: i64,
    sequence: u64,
}

struct TaskReservation {
    meta: Task,
    record: TaskRecord,
}

#[derive(Clone, Copy)]
struct CursorSecret {
    mask: u64,
    tag: u64,
}

pub(super) struct TaskManager {
    pub(super) tasks: HashMap<String, ManagedTask>,
    task_records: HashMap<String, TaskRecord>,
    reservations: HashMap<String, TaskReservation>,
    next_sequence: u64,
    clock: Clock,
    cursor_secret: CursorSecret,
    /// Per-account async mutex that serializes destructive tasks.
    /// These must be `tokio::sync::Mutex` because the guard is held across
    /// the entire `call_tool().await` execution.
    /// Non-destructive tasks bypass these locks entirely.
    pub(super) destructive_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl TaskManager {
    pub(super) fn new() -> Self {
        let secret = uuid::Uuid::new_v4().as_u128();
        Self::with_clock(
            Arc::new(Utc::now),
            CursorSecret {
                mask: secret as u64,
                tag: (secret >> 64) as u64,
            },
        )
    }

    fn with_clock(clock: Clock, cursor_secret: CursorSecret) -> Self {
        Self {
            tasks: HashMap::new(),
            task_records: HashMap::new(),
            reservations: HashMap::new(),
            next_sequence: 1,
            clock,
            cursor_secret,
            destructive_locks: HashMap::new(),
        }
    }

    /// Reserve bounded capacity before spawning work.
    ///
    /// The returned metadata already contains the creation-based TTL. Call
    /// [`Self::commit_task`] immediately after spawning the worker.
    pub(super) fn reserve_task(&mut self, task_id: String) -> Result<Task, McpError> {
        self.prune_expired();

        if self.tasks.len() + self.reservations.len() >= MAX_LIVE_TASKS {
            return Err(McpError::invalid_request(
                format!("task capacity reached ({MAX_LIVE_TASKS} live tasks)"),
                None,
            ));
        }
        if self.tasks.contains_key(&task_id) || self.reservations.contains_key(&task_id) {
            return Err(McpError::internal_error("duplicate task id", None));
        }

        let now = self.now();
        let timestamp = format_timestamp(&now);
        let task = Task::new(
            task_id.clone(),
            TaskStatus::Working,
            timestamp.clone(),
            timestamp,
        )
        .with_ttl(TASK_TTL_MS)
        .with_poll_interval(TASK_POLL_INTERVAL_MS);
        let record = TaskRecord {
            created_at_millis: now.timestamp_millis(),
            sequence: self.next_sequence,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.reservations.insert(
            task_id,
            TaskReservation {
                meta: task.clone(),
                record,
            },
        );
        Ok(task)
    }

    /// Attach a spawned worker to a prior capacity reservation.
    pub(super) fn commit_task(
        &mut self,
        task_id: &str,
        result: Arc<parking_lot::Mutex<Option<Result<CallToolResult, McpError>>>>,
        cancel: CancellationToken,
        handle: JoinHandle<()>,
    ) -> Result<Task, McpError> {
        self.prune_expired();
        let Some(reservation) = self.reservations.remove(task_id) else {
            cancel.cancel();
            handle.abort();
            return Err(McpError::internal_error(
                "task reservation is missing or expired",
                None,
            ));
        };

        let task = reservation.meta.clone();
        self.task_records
            .insert(task_id.to_string(), reservation.record);
        self.tasks.insert(
            task_id.to_string(),
            ManagedTask {
                meta: reservation.meta,
                result,
                cancel,
                handle,
            },
        );
        Ok(task)
    }

    /// Get or create the destructive-task serialization lock for an account.
    pub(super) fn destructive_lock(&mut self, account: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.prune_expired();
        self.destructive_locks
            .entry(account.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Remove expired entries and cancel their work before releasing metadata.
    pub(super) fn prune_expired(&mut self) {
        let now_millis = self.now().timestamp_millis();

        let expired_tasks: Vec<String> = self
            .tasks
            .iter()
            .filter_map(|(task_id, managed)| {
                let created_at_millis = self.task_records.get(task_id).map_or_else(
                    || parse_created_at(&managed.meta),
                    |record| record.created_at_millis,
                );
                is_expired(created_at_millis, now_millis).then(|| task_id.clone())
            })
            .collect();

        for task_id in expired_tasks {
            if let Some(managed) = self.tasks.remove(&task_id)
                && is_active(&managed.meta.status)
            {
                managed.cancel.cancel();
                managed.handle.abort();
            }
            self.task_records.remove(&task_id);
        }

        self.reservations
            .retain(|_, reservation| !is_expired(reservation.record.created_at_millis, now_millis));
        self.destructive_locks
            .retain(|_, lock| Arc::strong_count(lock) > 1);
    }

    /// Check if a specific task's spawned future has finished and update its
    /// metadata accordingly.
    pub(super) fn refresh_status(&mut self, task_id: &str) {
        self.prune_expired();
        let updated_at = format_timestamp(&self.now());
        if let Some(managed) = self.tasks.get_mut(task_id) {
            refresh_managed_task(managed, &updated_at);
        }
    }

    /// Refresh the status of all tracked tasks.
    pub(super) fn refresh_all(&mut self) {
        self.prune_expired();
        let updated_at = format_timestamp(&self.now());
        for managed in self.tasks.values_mut() {
            refresh_managed_task(managed, &updated_at);
        }
    }

    /// Return current task metadata after expiry and completion checks.
    pub(super) fn task_info(&mut self, task_id: &str) -> Result<Task, McpError> {
        self.refresh_status(task_id);
        self.tasks
            .get(task_id)
            .map(|managed| managed.meta.clone())
            .ok_or_else(|| unknown_task(task_id))
    }

    /// Clone a terminal task result, retaining it for repeat retrieval until expiry.
    pub(super) fn task_result(&mut self, task_id: &str) -> Result<CallToolResult, McpError> {
        self.refresh_status(task_id);
        let managed = self
            .tasks
            .get(task_id)
            .ok_or_else(|| unknown_task(task_id))?;
        match managed.meta.status {
            TaskStatus::Working => {
                return Err(McpError::invalid_params("task is still running", None));
            }
            TaskStatus::InputRequired => {
                return Err(McpError::invalid_params("task requires input", None));
            }
            TaskStatus::Cancelled => {
                return Err(McpError::invalid_params("task was cancelled", None));
            }
            TaskStatus::Completed | TaskStatus::Failed => {}
        }

        managed
            .result
            .try_lock()
            .and_then(|guard| guard.as_ref().cloned())
            .ok_or_else(|| McpError::internal_error("task result is unavailable", None))?
    }

    /// Cancel a non-terminal task and return its retained metadata.
    pub(super) fn cancel_task(&mut self, task_id: &str) -> Result<Task, McpError> {
        self.prune_expired();
        let updated_at = format_timestamp(&self.now());
        let managed = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| unknown_task(task_id))?;
        if is_active(&managed.meta.status) {
            managed.cancel.cancel();
            managed.handle.abort();
            managed.meta.status = TaskStatus::Cancelled;
            managed.meta.last_updated_at = updated_at;
        }
        Ok(managed.meta.clone())
    }

    /// List retained tasks newest-first with a process-local opaque cursor.
    pub(super) fn list_page(&mut self, cursor: Option<&str>) -> Result<ListTasksResult, McpError> {
        self.refresh_all();
        let offset = cursor.map_or(Ok(0), |value| self.decode_cursor(value))?;

        let mut entries: Vec<(&String, &ManagedTask)> = self.tasks.iter().collect();
        entries.sort_by(|(left_id, left), (right_id, right)| {
            let left_record = self.record_for(left_id, left);
            let right_record = self.record_for(right_id, right);
            right_record
                .sequence
                .cmp(&left_record.sequence)
                .then_with(|| {
                    right_record
                        .created_at_millis
                        .cmp(&left_record.created_at_millis)
                })
                .then_with(|| right_id.cmp(left_id))
        });

        let total = entries.len();
        let tasks = entries
            .iter()
            .skip(offset)
            .take(TASK_PAGE_SIZE)
            .map(|(_, managed)| managed.meta.clone())
            .collect();
        let page_end = offset.saturating_add(TASK_PAGE_SIZE).min(total);
        let next_cursor = (page_end < total).then(|| self.encode_cursor(page_end));

        let mut result = ListTasksResult::new(tasks);
        result.next_cursor = next_cursor;
        result.total = Some(total as u64);
        Ok(result)
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    fn record_for(&self, task_id: &str, managed: &ManagedTask) -> TaskRecord {
        self.task_records
            .get(task_id)
            .copied()
            .unwrap_or(TaskRecord {
                created_at_millis: parse_created_at(&managed.meta),
                sequence: 0,
            })
    }

    fn encode_cursor(&self, offset: usize) -> String {
        let masked = (offset as u64) ^ self.cursor_secret.mask;
        let tag = mix64(masked ^ self.cursor_secret.tag);
        format!("{TASK_CURSOR_PREFIX}.{masked:016x}.{tag:016x}")
    }

    fn decode_cursor(&self, cursor: &str) -> Result<usize, McpError> {
        let mut parts = cursor.split('.');
        let Some(prefix) = parts.next() else {
            return Err(invalid_cursor());
        };
        let Some(masked) = parts.next() else {
            return Err(invalid_cursor());
        };
        let Some(tag) = parts.next() else {
            return Err(invalid_cursor());
        };
        if prefix != TASK_CURSOR_PREFIX || parts.next().is_some() {
            return Err(invalid_cursor());
        }

        let masked = u64::from_str_radix(masked, 16).map_err(|_| invalid_cursor())?;
        let tag = u64::from_str_radix(tag, 16).map_err(|_| invalid_cursor())?;
        if tag != mix64(masked ^ self.cursor_secret.tag) {
            return Err(invalid_cursor());
        }
        let offset = masked ^ self.cursor_secret.mask;
        let offset = usize::try_from(offset).map_err(|_| invalid_cursor())?;
        if offset > MAX_LIVE_TASKS {
            return Err(invalid_cursor());
        }
        Ok(offset)
    }
}

fn refresh_managed_task(managed: &mut ManagedTask, updated_at: &str) {
    if !is_active(&managed.meta.status) || !managed.handle.is_finished() {
        return;
    }

    let Some(result) = managed.result.try_lock() else {
        return;
    };
    managed.meta.status = match result.as_ref() {
        Some(Ok(call_result)) if call_result.is_error == Some(true) => TaskStatus::Failed,
        Some(Ok(_)) => TaskStatus::Completed,
        Some(Err(_)) | None => TaskStatus::Failed,
    };
    managed.meta.last_updated_at = updated_at.to_string();
}

fn is_active(status: &TaskStatus) -> bool {
    matches!(status, TaskStatus::Working | TaskStatus::InputRequired)
}

fn is_expired(created_at_millis: i64, now_millis: i64) -> bool {
    now_millis.saturating_sub(created_at_millis) >= TASK_TTL_MS as i64
}

fn parse_created_at(task: &Task) -> i64 {
    DateTime::parse_from_rfc3339(&task.created_at)
        .map(|timestamp| timestamp.timestamp_millis())
        .unwrap_or(i64::MIN)
}

fn format_timestamp(timestamp: &DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn unknown_task(task_id: &str) -> McpError {
    McpError::invalid_params(format!("unknown or expired task: {task_id}"), None)
}

fn invalid_cursor() -> McpError {
    McpError::invalid_params("invalid task cursor", None)
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use rmcp::model::Content;

    use super::*;

    const START_MILLIS: i64 = 1_750_000_000_000;

    #[derive(Clone)]
    struct TestClock(Arc<AtomicI64>);

    impl TestClock {
        fn new() -> Self {
            Self(Arc::new(AtomicI64::new(START_MILLIS)))
        }

        fn advance(&self, millis: u64) {
            self.0.fetch_add(millis as i64, Ordering::SeqCst);
        }

        fn clock(&self) -> Clock {
            let millis = Arc::clone(&self.0);
            Arc::new(move || {
                DateTime::from_timestamp_millis(millis.load(Ordering::SeqCst))
                    .expect("test timestamp should be valid")
            })
        }
    }

    fn manager(clock: &TestClock) -> TaskManager {
        TaskManager::with_clock(
            clock.clock(),
            CursorSecret {
                mask: 0x1234_5678_9abc_def0,
                tag: 0x0fed_cba9_8765_4321,
            },
        )
    }

    fn commit_completed(manager: &mut TaskManager, task_id: &str) -> Task {
        manager
            .reserve_task(task_id.to_string())
            .expect("task reservation should succeed");
        let result = CallToolResult::success(vec![Content::text(task_id.to_string())]);
        let result = Arc::new(parking_lot::Mutex::new(Some(Ok(result))));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(async {});
        let task = manager
            .commit_task(task_id, result, cancel, handle)
            .expect("task commit should succeed");
        manager
            .tasks
            .get_mut(task_id)
            .expect("committed task should exist")
            .meta
            .status = TaskStatus::Completed;
        task
    }

    fn commit_running(manager: &mut TaskManager, task_id: &str) -> CancellationToken {
        manager
            .reserve_task(task_id.to_string())
            .expect("task reservation should succeed");
        let result = Arc::new(parking_lot::Mutex::new(None));
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            worker_cancel.cancelled().await;
        });
        manager
            .commit_task(task_id, result, cancel.clone(), handle)
            .expect("task commit should succeed");
        cancel
    }

    #[tokio::test]
    async fn reserve_task_should_advertise_creation_based_24_hour_ttl() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);

        let task = manager
            .reserve_task("ttl".to_string())
            .expect("task reservation should succeed");

        assert_eq!(task.ttl, Some(TASK_TTL_MS));
    }

    #[tokio::test]
    async fn prune_expired_should_cancel_running_work_at_ttl_boundary() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        let cancellation = commit_running(&mut manager, "running");

        clock.advance(TASK_TTL_MS);
        let error = manager
            .task_info("running")
            .expect_err("expired task should be unavailable");

        assert!(
            cancellation.is_cancelled() && error.message.contains("expired"),
            "expired running task should be cancelled and reported unavailable"
        );
    }

    #[tokio::test]
    async fn expiry_should_be_measured_from_creation_not_last_update() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        commit_running(&mut manager, "cancelled");

        clock.advance(TASK_TTL_MS / 2);
        manager
            .cancel_task("cancelled")
            .expect("live task cancellation should succeed");
        clock.advance(TASK_TTL_MS / 2);

        assert!(
            manager.task_info("cancelled").is_err(),
            "lastUpdatedAt must not extend retention"
        );
    }

    #[tokio::test]
    async fn reserve_task_should_reject_capacity_until_entries_expire() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        for index in 0..MAX_LIVE_TASKS {
            commit_completed(&mut manager, &format!("task-{index:03}"));
        }

        let error = manager
            .reserve_task("overflow".to_string())
            .expect_err("live capacity should be enforced");
        clock.advance(TASK_TTL_MS);
        let after_expiry = manager.reserve_task("after-expiry".to_string());

        assert!(
            error.message.contains("capacity") && after_expiry.is_ok(),
            "capacity should reject live entries and recover after pruning"
        );
    }

    #[tokio::test]
    async fn task_result_should_be_repeatable_until_expiry() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        commit_completed(&mut manager, "repeatable");

        let first = manager
            .task_result("repeatable")
            .expect("first result retrieval should succeed");
        let second = manager
            .task_result("repeatable")
            .expect("second result retrieval should succeed");

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn list_page_should_return_newest_first_in_pages_of_25() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        for index in 0..27 {
            commit_completed(&mut manager, &format!("task-{index:02}"));
            clock.advance(1);
        }

        let first = manager
            .list_page(None)
            .expect("first task page should succeed");
        let second = manager
            .list_page(first.next_cursor.as_deref())
            .expect("second task page should succeed");

        assert!(
            first.tasks.len() == TASK_PAGE_SIZE
                && first.tasks[0].task_id == "task-26"
                && first.total == Some(27)
                && second.tasks.len() == 2
                && second.tasks[0].task_id == "task-01"
                && second.next_cursor.is_none(),
            "task pages should be bounded, newest-first, and complete"
        );
    }

    #[tokio::test]
    async fn list_page_should_reject_malformed_or_tampered_cursor() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);

        let error = manager
            .list_page(Some("am-task-v1.0000000000000001.0000000000000001"))
            .expect_err("tampered cursor should be rejected");

        assert!(error.message.contains("invalid task cursor"));
    }
}
