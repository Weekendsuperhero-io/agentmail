//! Task management (SEP-1686): background execution of long-running tools.

use super::utc_now_iso8601;
use hashbrown::HashMap;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Task, TaskStatus};
use std::sync::Arc;
use tokio::task::JoinHandle;

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
    pub(super) handle: JoinHandle<()>,
}

pub(super) struct TaskManager {
    pub(super) tasks: HashMap<String, ManagedTask>,
    /// Per-account async mutex that serializes destructive tasks.
    /// These must be `tokio::sync::Mutex` because the guard is held across
    /// the entire `call_tool().await` execution.
    /// Non-destructive tasks bypass these locks entirely.
    pub(super) destructive_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl TaskManager {
    pub(super) fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            destructive_locks: HashMap::new(),
        }
    }

    /// Get or create the destructive-task serialization lock for an account.
    pub(super) fn destructive_lock(&mut self, account: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.destructive_locks
            .entry(account.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Check if a specific task's spawned future has finished and update its
    /// metadata accordingly.
    pub(super) fn refresh_status(&mut self, task_id: &str) {
        if let Some(managed) = self.tasks.get_mut(task_id) {
            if managed.meta.status != TaskStatus::Working {
                return;
            }
            if managed.handle.is_finished() {
                let now = utc_now_iso8601();
                // Try to determine if it succeeded or failed by checking the
                // result slot synchronously.
                let status = match managed.result.try_lock() {
                    Some(guard) => match guard.as_ref() {
                        Some(Ok(_)) => TaskStatus::Completed,
                        Some(Err(_)) => TaskStatus::Failed,
                        None => {
                            // Handle finished but no result written → aborted/panicked
                            TaskStatus::Failed
                        }
                    },
                    None => {
                        // Lock contention — treat as still completing
                        return;
                    }
                };
                managed.meta.status = status;
                managed.meta.last_updated_at = now;
            }
        }
    }

    /// Refresh the status of all tracked tasks.
    pub(super) fn refresh_all(&mut self) {
        let ids: Vec<String> = self.tasks.keys().cloned().collect();
        for id in ids {
            self.refresh_status(&id);
        }
    }
}
