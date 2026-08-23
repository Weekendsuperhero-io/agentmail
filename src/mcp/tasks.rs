//! Task management (SEP-1686): background execution of long-running tools.

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use hashbrown::HashMap;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock, ListTasksResult, Task, TaskStatus};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Task results are retained for 24 hours from task creation.
pub(super) const TASK_TTL_MS: u64 = 86_400_000;
/// Maximum number of concurrently active tasks and enqueue reservations.
pub(super) const MAX_ACTIVE_TASKS: usize = 128;
/// Maximum accepted tasks retained at once, active and terminal combined.
/// Admission stops at this ceiling so no accepted result is evicted before
/// its advertised TTL expires.
pub(super) const MAX_TRACKED_TASKS: usize = 1_024;
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
    "delete_mailbox",
    "delete_by_sender",
    "delete_by_domain",
    "delete_list_id",
    "unsubscribe_message",
    "reconcile_moves",
    "rename_mailbox",
    "update_draft",
];

/// Try to extract the `account` field from a tool call's JSON arguments.
pub(super) fn extract_account(
    args: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    args.as_ref()?.get("account")?.as_str().map(String::from)
}

#[derive(Default)]
pub(super) struct TaskCompletion {
    result: parking_lot::Mutex<Option<Result<CallToolResult, McpError>>>,
    changed: Notify,
}

impl TaskCompletion {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Publish the worker result once. A cancellation result wins races with
    /// a worker that finishes after cancellation.
    pub(super) fn complete(&self, result: Result<CallToolResult, McpError>) {
        let mut slot = self.result.lock();
        if slot.is_none() {
            *slot = Some(result);
            drop(slot);
            self.changed.notify_waiters();
        }
    }

    fn cancel(&self, task_id: &str) {
        let mut slot = self.result.lock();
        *slot = Some(Ok(CallToolResult::error(vec![ContentBlock::text(
            format!("Task {task_id} was cancelled before completion."),
        )])));
        drop(slot);
        self.changed.notify_waiters();
    }

    fn snapshot(&self) -> Option<Result<CallToolResult, McpError>> {
        self.result.lock().clone()
    }

    /// Wait until the task reaches a result-bearing terminal state. Cancelling
    /// this particular `tasks/result` request does not cancel the task itself.
    pub(super) async fn wait(
        &self,
        request_cancel: &CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = self.snapshot() {
                return result;
            }
            tokio::select! {
                () = &mut changed => {}
                () = request_cancel.cancelled() => {
                    return Err(McpError::internal_error(
                        "tasks/result request was cancelled",
                        None,
                    ));
                }
            }
        }
    }
}

pub(super) struct ManagedTask {
    pub(super) meta: Task,
    pub(super) completion: Arc<TaskCompletion>,
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
        self.refresh_all();

        if self.tasks.len() + self.reservations.len() >= MAX_TRACKED_TASKS {
            return Err(McpError::internal_error(
                format!("task retention capacity reached ({MAX_TRACKED_TASKS} tasks)"),
                None,
            ));
        }
        let active = self
            .tasks
            .values()
            .filter(|managed| !is_terminal(&managed.meta.status))
            .count();
        if active + self.reservations.len() >= MAX_ACTIVE_TASKS {
            return Err(McpError::internal_error(
                format!("task capacity reached ({MAX_ACTIVE_TASKS} active tasks)"),
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
        completion: Arc<TaskCompletion>,
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
                completion,
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
                && !is_terminal(&managed.meta.status)
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

    /// Return the completion signal used by blocking `tasks/result` calls.
    pub(super) fn task_completion(
        &mut self,
        task_id: &str,
    ) -> Result<Arc<TaskCompletion>, McpError> {
        self.refresh_status(task_id);
        self.tasks
            .get(task_id)
            .map(|managed| Arc::clone(&managed.completion))
            .ok_or_else(|| unknown_task(task_id))
    }

    /// Cancel a non-terminal task and return its retained metadata.
    pub(super) fn cancel_task(&mut self, task_id: &str) -> Result<Task, McpError> {
        self.refresh_status(task_id);
        let updated_at = format_timestamp(&self.now());
        let managed = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| unknown_task(task_id))?;
        if is_terminal(&managed.meta.status) {
            return Err(McpError::invalid_params(
                format!("task is already terminal: {task_id}"),
                None,
            ));
        }
        managed.cancel.cancel();
        managed.completion.cancel(task_id);
        managed.handle.abort();
        managed.meta.status = TaskStatus::Cancelled;
        managed.meta.status_message = Some("The task was cancelled by request.".to_string());
        managed.meta.last_updated_at = updated_at;
        Ok(managed.meta.clone())
    }

    /// List retained tasks newest-first with a process-local opaque cursor.
    pub(super) fn list_page(&mut self, cursor: Option<&str>) -> Result<ListTasksResult, McpError> {
        self.refresh_all();
        let before_sequence = cursor.map(|value| self.decode_cursor(value)).transpose()?;

        let mut entries: Vec<(&String, &ManagedTask)> = self.tasks.iter().collect();
        if let Some(before_sequence) = before_sequence {
            entries.retain(|(task_id, managed)| {
                self.record_for(task_id, managed).sequence < before_sequence
            });
        }
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

        let has_more = entries.len() > TASK_PAGE_SIZE;
        let page = entries
            .iter()
            .take(TASK_PAGE_SIZE)
            .copied()
            .collect::<Vec<_>>();
        let next_cursor = has_more.then(|| {
            let (task_id, managed) = page
                .last()
                .expect("a page with more rows always has a final row");
            self.encode_cursor(self.record_for(task_id, managed).sequence)
        });
        let tasks = page
            .into_iter()
            .map(|(_, managed)| managed.meta.clone())
            .collect();

        let mut result = ListTasksResult::new(tasks);
        result.next_cursor = next_cursor;
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

    fn encode_cursor(&self, before_sequence: u64) -> String {
        let masked = before_sequence ^ self.cursor_secret.mask;
        let tag = mix64(masked ^ self.cursor_secret.tag);
        format!("{TASK_CURSOR_PREFIX}.{masked:016x}.{tag:016x}")
    }

    fn decode_cursor(&self, cursor: &str) -> Result<u64, McpError> {
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
        Ok(masked ^ self.cursor_secret.mask)
    }
}

fn refresh_managed_task(managed: &mut ManagedTask, updated_at: &str) {
    if is_terminal(&managed.meta.status) {
        return;
    }

    let result = managed.completion.snapshot();
    if result.is_none() && !managed.handle.is_finished() {
        return;
    }
    if result.is_none() {
        managed.completion.complete(Err(McpError::internal_error(
            "task worker ended without publishing a result",
            None,
        )));
    }
    let result = managed.completion.snapshot();
    managed.meta.status = match result.as_ref() {
        Some(Ok(call_result)) if call_result.is_error == Some(true) => TaskStatus::Failed,
        Some(Ok(_)) => TaskStatus::Completed,
        Some(Err(_)) | None => TaskStatus::Failed,
    };
    managed.meta.status_message = match result {
        Some(Err(error)) => Some(error.message.to_string()),
        Some(Ok(ref call_result)) if call_result.is_error == Some(true) => call_result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|text| text.text.clone()),
        _ => None,
    };
    managed.meta.last_updated_at = updated_at.to_string();
}

fn is_terminal(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
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

    use rmcp::model::ContentBlock;
    use tokio::sync::oneshot;

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
        let result = CallToolResult::success(vec![ContentBlock::text(task_id.to_string())]);
        let completion = Arc::new(TaskCompletion::new());
        completion.complete(Ok(result));
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(async {});
        let task = manager
            .commit_task(task_id, completion, cancel, handle)
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
        let completion = Arc::new(TaskCompletion::new());
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            worker_cancel.cancelled().await;
        });
        manager
            .commit_task(task_id, completion, cancel.clone(), handle)
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
    async fn task_completion_should_block_until_worker_publishes() {
        let completion = Arc::new(TaskCompletion::new());
        let waiting_completion = Arc::clone(&completion);
        let waiter =
            tokio::spawn(async move { waiting_completion.wait(&CancellationToken::new()).await });
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "tasks/result must block while work is active"
        );

        completion.complete(Ok(CallToolResult::success(vec![ContentBlock::text(
            "done",
        )])));
        let result = waiter
            .await
            .expect("waiter should not panic")
            .expect("published result should be returned");

        assert_eq!(result.is_error, Some(false));
    }

    #[tokio::test]
    async fn cancelled_task_should_retain_a_final_error_result() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        commit_running(&mut manager, "cancelled-result");
        let completion = manager
            .task_completion("cancelled-result")
            .expect("task should be present");

        let task = manager
            .cancel_task("cancelled-result")
            .expect("active task should be cancellable");
        let result = completion
            .wait(&CancellationToken::new())
            .await
            .expect("cancelled task should have a final result");

        assert!(
            task.status == TaskStatus::Cancelled && result.is_error == Some(true),
            "cancelled status and final isError result must agree"
        );
    }

    #[tokio::test]
    async fn cancelling_terminal_task_should_return_invalid_params() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        commit_completed(&mut manager, "completed");

        let error = manager
            .cancel_task("completed")
            .expect_err("terminal tasks cannot be cancelled");

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn completed_tasks_should_not_consume_active_capacity() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        for index in 0..MAX_ACTIVE_TASKS {
            commit_completed(&mut manager, &format!("task-{index:03}"));
        }

        let reservation = manager.reserve_task("new-active".to_string());

        assert!(
            reservation.is_ok(),
            "terminal retention must not block new work"
        );
    }

    #[tokio::test]
    async fn retention_capacity_should_reject_admission_without_evicting_promised_results() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        for index in 0..MAX_TRACKED_TASKS {
            commit_completed(&mut manager, &format!("retained-{index:04}"));
        }

        let error = manager
            .reserve_task("overflow".to_string())
            .expect_err("retention capacity should reject new tasks");

        assert!(error.message.contains("retention capacity"));
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            manager.task_info("retained-0000").is_ok(),
            "accepted task results must remain available until their TTL"
        );
    }

    #[tokio::test]
    async fn active_tasks_should_enforce_capacity() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        for index in 0..MAX_ACTIVE_TASKS {
            commit_running(&mut manager, &format!("task-{index:03}"));
        }

        let error = manager
            .reserve_task("overflow".to_string())
            .expect_err("active capacity should be enforced");

        assert!(error.message.contains("active tasks"));
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn task_result_should_be_repeatable_until_expiry() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        commit_completed(&mut manager, "repeatable");

        let completion = manager
            .task_completion("repeatable")
            .expect("task completion should be available");
        let first = completion
            .wait(&CancellationToken::new())
            .await
            .expect("first result retrieval should succeed");
        let second = completion
            .wait(&CancellationToken::new())
            .await
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
                && second.tasks.len() == 2
                && second.tasks[0].task_id == "task-01"
                && second.next_cursor.is_none(),
            "task pages should be bounded, newest-first, and complete"
        );
    }

    #[tokio::test]
    async fn list_cursor_should_not_shift_when_new_tasks_arrive_between_pages() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        for index in 0..27 {
            commit_completed(&mut manager, &format!("task-{index:02}"));
            clock.advance(1);
        }

        let first = manager.list_page(None).expect("first page");
        commit_completed(&mut manager, "newer-task");
        let second = manager
            .list_page(first.next_cursor.as_deref())
            .expect("second page");

        assert_eq!(
            second
                .tasks
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-01", "task-00"],
            "the opaque cursor must continue after the prior page boundary"
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

    #[test]
    fn destructive_lock_should_return_same_mutex_per_account() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);

        let work_first = manager.destructive_lock("work");
        let work_second = manager.destructive_lock("work");
        let personal = manager.destructive_lock("personal");

        assert!(
            Arc::ptr_eq(&work_first, &work_second) && !Arc::ptr_eq(&work_first, &personal),
            "same account shares one serialization lock; different accounts get distinct locks"
        );
    }

    /// Two destructive tasks on the same account run strictly one after the
    /// other. Mirrors `enqueue_task`'s wiring: each spawned worker holds the
    /// account's destructive lock across its tool call and is registered via
    /// `reserve_task`/`commit_task`.
    #[tokio::test]
    async fn destructive_tasks_should_serialize_on_same_account() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        let lock = manager.destructive_lock("work");
        let events: Arc<parking_lot::Mutex<Vec<&'static str>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (started_tx, started_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let (done_tx, mut done_rx) = oneshot::channel();

        manager
            .reserve_task("first".to_string())
            .expect("task reservation should succeed");
        let first_completion = Arc::new(TaskCompletion::new());
        let worker_completion = Arc::clone(&first_completion);
        let worker_lock = Arc::clone(&lock);
        let worker_events = Arc::clone(&events);
        let first_handle = tokio::spawn(async move {
            let _guard = worker_lock.lock().await;
            worker_events.lock().push("first:acquired");
            started_tx
                .send(())
                .expect("test should await the start signal");
            finish_rx.await.expect("test should signal completion");
            worker_events.lock().push("first:releasing");
            worker_completion.complete(Ok(CallToolResult::success(vec![ContentBlock::text(
                "first",
            )])));
        });
        manager
            .commit_task(
                "first",
                first_completion,
                CancellationToken::new(),
                first_handle,
            )
            .expect("task commit should succeed");

        started_rx
            .await
            .expect("first worker should acquire the lock");
        // The first worker provably holds the lock; only now enqueue the
        // second so the acquisition order is fixed.
        manager
            .reserve_task("second".to_string())
            .expect("task reservation should succeed");
        let second_completion = Arc::new(TaskCompletion::new());
        let worker_completion = Arc::clone(&second_completion);
        let worker_lock = Arc::clone(&lock);
        let worker_events = Arc::clone(&events);
        let second_handle = tokio::spawn(async move {
            let _guard = worker_lock.lock().await;
            worker_events.lock().push("second:acquired");
            worker_completion.complete(Ok(CallToolResult::success(vec![ContentBlock::text(
                "second",
            )])));
            done_tx.send(()).expect("test should await the done signal");
        });
        manager
            .commit_task(
                "second",
                second_completion,
                CancellationToken::new(),
                second_handle,
            )
            .expect("task commit should succeed");

        // Give the second worker every chance to (incorrectly) run: on the
        // current-thread test runtime each yield lets every ready task reach
        // its next await point.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            lock.try_lock().is_err(),
            "account lock should still be held by the first worker"
        );
        assert!(
            matches!(done_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "second destructive task must not run while the first holds the lock"
        );

        finish_tx
            .send(())
            .expect("first worker should be waiting for the finish signal");
        done_rx
            .await
            .expect("second worker should acquire after the first releases");

        (&mut manager
            .tasks
            .get_mut("first")
            .expect("first task should be tracked")
            .handle)
            .await
            .expect("first worker should not panic");
        (&mut manager
            .tasks
            .get_mut("second")
            .expect("second task should be tracked")
            .handle)
            .await
            .expect("second worker should not panic");

        assert_eq!(
            events.lock().clone(),
            vec!["first:acquired", "first:releasing", "second:acquired"],
            "second destructive task starts only after the first releases the account lock"
        );
    }

    #[tokio::test]
    async fn destructive_lock_should_not_block_other_accounts() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);
        let work = manager.destructive_lock("work");
        let personal = manager.destructive_lock("personal");

        let _work_guard = work.lock().await;

        assert!(
            personal.try_lock().is_ok(),
            "holding one account's destructive lock must not block another account"
        );
    }

    #[test]
    fn prune_expired_should_drop_destructive_locks_once_unused() {
        let clock = TestClock::new();
        let mut manager = manager(&clock);

        // The local clone stands in for a worker's clone: retention depends
        // only on the Arc's strong count.
        let lock = manager.destructive_lock("work");
        manager.prune_expired();
        assert!(
            manager.destructive_locks.contains_key("work"),
            "an outstanding clone keeps the account lock registered"
        );

        drop(lock);
        manager.prune_expired();
        assert!(
            !manager.destructive_locks.contains_key("work"),
            "unused destructive locks are pruned"
        );
    }

    #[test]
    fn extract_account_should_read_string_account_argument() {
        let mut args = serde_json::Map::new();
        args.insert("account".to_string(), serde_json::json!("work"));

        assert_eq!(extract_account(&Some(args)).as_deref(), Some("work"));
    }

    #[test]
    fn extract_account_should_reject_missing_or_non_string_account() {
        let mut non_string = serde_json::Map::new();
        non_string.insert("account".to_string(), serde_json::json!(7));

        assert!(
            extract_account(&None).is_none()
                && extract_account(&Some(serde_json::Map::new())).is_none()
                && extract_account(&Some(non_string)).is_none(),
            "account extraction should require a string account value"
        );
    }
}
