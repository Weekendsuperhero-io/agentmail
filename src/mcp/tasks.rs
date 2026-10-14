//! MCP Tasks (SEP-2663, the `io.modelcontextprotocol/tasks` extension): a slow
//! tool call becomes a task the client polls, instead of a request it has to
//! hold open past its own timeout.
//!
//! **The server decides, and decides late.** A call becomes a task only when
//! all four hold: the tool is one of [`TASK_TOOLS`], the client declared the
//! extension, the call is still running after [`TaskConfig::promote_after`],
//! and admission ([`TaskConfig::max_active`], [`TaskConfig::max_tracked`])
//! lets it in. Anything else runs exactly as a plain call always has, so a
//! client that never declared tasks sees no difference at all. Promoting late
//! rather than deciding up front keeps the quick calls quick: a task costs the
//! client a `tasks/get` round trip and keeps its result for hours, which is
//! only worth it for a call that would have outlived a request anyway.
//!
//! **The token rule.** rmcp cancels a request's token when it sends that
//! request's response (`Event::ToSink` in its service loop). A task's response
//! is the `CreateTaskResult`, sent the moment the call is promoted, so an
//! operation running on the request's token would cancel itself right there.
//! The SEP-1686 tasks this crate retired in 0.8.0 had exactly that bug until
//! their token was re-parented. Every promotable operation therefore runs on a
//! child of the server's own lifetime token, linked to the request's token
//! only while it is still a request.
//!
//! **Cancellation is cooperative.** `tasks/cancel` cancels the operation's
//! token, and the operation stops at its next check, never mid-write. Its
//! terminal status follows what it did: `cancelled` when it saw the
//! cancellation, `completed` when it finished first (SEP-2663 lets a task
//! reach a non-`cancelled` status after a cancel). rmcp's TTL is the only hard
//! stop.
//!
//! **Progress.** Before promotion a call reports `notifications/progress` on
//! the request's token, as it always did. After, that request has been
//! answered, so progress becomes the task's status message, and `tasks/get`
//! carries the numbers under [`PROGRESS_META_KEY`] for a host that relays them
//! as progress of its own.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData as McpError,
    model::{
        CallToolResponse, CallToolResult, CreateTaskResult, GetTaskResult, InputResponses,
        MetaObject, Task,
    },
    task_manager::{TaskContext, TaskExit, TaskManager, TaskOptions},
};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

/// The tools a call to which may become a task: the ones that can outlive a
/// request. Account-wide scans, bulk moves and deletes, unsubscribing (which
/// can clean up afterwards), downloads and thread exports. A lookup that
/// answers from one mailbox page never needs one.
pub(super) const TASK_TOOLS: &[&str] = &[
    "search_messages",
    "list_flags",
    "find_attachments",
    "top_senders",
    "top_domains",
    "top_subscriptions",
    "top_mailing_lists",
    "preview_thread_record",
    "delete_messages",
    "delete_by_sender",
    "delete_by_domain",
    "delete_list_id",
    "move_by_sender",
    "move_by_domain",
    "move_list_id",
    "move_subscription",
    "unsubscribe_message",
    "reconcile_moves",
    "download_attachments",
    "download_message_source",
    "download_thread",
    "export_thread_record",
];

/// The `_meta` key under which `tasks/get` reports a running task's latest
/// progress, as `{"progress": n, "total": m}`. The request's own progress
/// token died with the request, so this is how a host keeps relaying it.
pub(super) const PROGRESS_META_KEY: &str = "io.agentmuse/progress";

#[derive(Debug, Clone, Copy)]
pub(super) struct TaskConfig {
    /// How long a call runs as a plain request before it may become a task.
    pub(super) promote_after: Duration,
    /// rmcp's hard stop: a task still running this long after it was created
    /// is aborted and marked `failed`, and a finished one is kept this long
    /// after it ends.
    pub(super) ttl: Duration,
    /// The polling interval the task suggests to the client.
    pub(super) poll_interval: Duration,
    /// Tasks running at once. A call past this stays a request.
    pub(super) max_active: usize,
    /// Tasks held at once, running and finished, so finished results cannot
    /// pile up for a whole TTL. A call past this stays a request.
    pub(super) max_tracked: usize,
    /// How long running tasks get to stop when the connection closes, before
    /// whatever is left is aborted.
    pub(super) shutdown_grace: Duration,
}

impl TaskConfig {
    pub(super) const DEFAULT: Self = Self {
        promote_after: Duration::from_secs(2),
        ttl: Duration::from_secs(4 * 60 * 60),
        poll_interval: Duration::from_secs(1),
        max_active: 32,
        max_tracked: 1_024,
        shutdown_grace: Duration::from_secs(5),
    };
}

tokio::task_local! {
    /// The promotable call the current tool is running for, if any.
    static TOOL_RUN: Arc<ToolRun>;
}

/// One promotable call, as seen by the progress and cancel callbacks its tool
/// builds ([`super::make_progress_fn`], [`super::make_cancel_fn`]).
#[derive(Default)]
pub(super) struct ToolRun {
    /// Set once, when the call is promoted: from then on it is a task.
    task: OnceLock<PromotedRun>,
    /// Whether the operation saw a cancellation through its `CancelFn`.
    cancel_observed: AtomicBool,
}

struct PromotedRun {
    context: TaskContext,
    progress: Arc<ProgressBoard>,
}

impl ToolRun {
    /// The call the current tool is running for, or `None` outside a
    /// promotable call.
    pub(super) fn current() -> Option<Arc<Self>> {
        TOOL_RUN.try_with(Arc::clone).ok()
    }

    pub(super) fn is_promoted(&self) -> bool {
        self.task.get().is_some()
    }

    /// Record a progress update on the task, once the call is one. Returns
    /// `false` while it is still a request, whose progress is a notification.
    pub(super) fn report_progress(&self, completed: u64, total: u64) -> bool {
        let Some(promoted) = self.task.get() else {
            return false;
        };
        promoted
            .context
            .set_status_message(format!("{completed} of {total}"));
        promoted
            .progress
            .record(promoted.context.task_id(), completed, total);
        true
    }

    pub(super) fn note_cancel_observed(&self) {
        self.cancel_observed.store(true, Ordering::Relaxed);
    }

    fn cancel_observed(&self) -> bool {
        self.cancel_observed.load(Ordering::Relaxed)
    }
}

/// The latest progress of each task, for `tasks/get`.
#[derive(Default)]
struct ProgressBoard(parking_lot::Mutex<HashMap<String, (u64, u64)>>);

impl ProgressBoard {
    fn record(&self, task_id: &str, completed: u64, total: u64) {
        self.0
            .lock()
            .insert(task_id.to_string(), (completed, total));
    }

    fn get(&self, task_id: &str) -> Option<(u64, u64)> {
        self.0.lock().get(task_id).copied()
    }

    fn forget(&self, task_id: &str) {
        self.0.lock().remove(task_id);
    }
}

/// The spawned operation behind a call. Aborted if dropped before it ends, so
/// nothing outlives the request or task that owns it.
struct Operation(JoinHandle<Result<CallToolResponse, McpError>>);

impl Operation {
    async fn join(&mut self) -> Result<Result<CallToolResponse, McpError>, JoinError> {
        (&mut self.0).await
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The tasks of one connection: the rmcp store, the lifetime every operation
/// token descends from, and the admission bounds.
pub(super) struct TaskRuntime {
    manager: TaskManager,
    lifetime: CancellationToken,
    config: TaskConfig,
    /// Tasks created here that rmcp may still hold, with when they were made.
    tracked: parking_lot::Mutex<VecDeque<(String, Instant)>>,
    progress: Arc<ProgressBoard>,
}

impl TaskRuntime {
    pub(super) fn new(config: TaskConfig) -> Self {
        Self {
            manager: TaskManager::new(),
            lifetime: CancellationToken::new(),
            config,
            tracked: parking_lot::Mutex::new(VecDeque::new()),
            progress: Arc::new(ProgressBoard::default()),
        }
    }

    /// A token for one operation: a child of this connection's lifetime, so
    /// it outlives the request that started it (see the module doc).
    pub(super) fn operation_token(&self) -> CancellationToken {
        self.lifetime.child_token()
    }

    /// Run one promotable call.
    ///
    /// `operation` runs on `operation_ct` (from [`Self::operation_token`]).
    /// While the call is a request, a cancellation of `request_ct` (a
    /// `notifications/cancelled`, or the connection closing) is passed on to
    /// the operation, which then stops at its next check exactly as a plain
    /// call does. Once promoted, the request's token is left behind: rmcp
    /// cancels it as it sends the `CreateTaskResult`.
    pub(super) async fn run_call<F>(
        &self,
        tool: &str,
        request_ct: CancellationToken,
        operation_ct: CancellationToken,
        operation: F,
    ) -> Result<CallToolResponse, McpError>
    where
        F: Future<Output = Result<CallToolResponse, McpError>> + Send + 'static,
    {
        let run = Arc::new(ToolRun::default());
        let mut operation = Operation(tokio::spawn(TOOL_RUN.scope(Arc::clone(&run), operation)));
        let promote_at = tokio::time::sleep(self.config.promote_after);
        tokio::pin!(promote_at);
        let mut promotable = true;
        loop {
            tokio::select! {
                biased;
                joined = operation.join() => return settle_request(joined),
                () = request_ct.cancelled() => {
                    operation_ct.cancel();
                    return settle_request(operation.join().await);
                }
                () = &mut promote_at, if promotable => {
                    match self.promote(tool, &run, operation, operation_ct.clone()) {
                        Ok(task) => {
                            return Ok(CallToolResponse::Task(CreateTaskResult::new(task)));
                        }
                        // Not admitted: the call carries on as a request.
                        Err(not_admitted) => {
                            operation = not_admitted;
                            promotable = false;
                        }
                    }
                }
            }
        }
    }

    /// Turn a running call into a task, or hand the operation back when
    /// admission refuses it.
    fn promote(
        &self,
        tool: &str,
        run: &Arc<ToolRun>,
        operation: Operation,
        operation_ct: CancellationToken,
    ) -> Result<Task, Operation> {
        let mut tracked = self.tracked.lock();
        if !self.admits(&mut tracked) {
            tracing::debug!(
                target: "agentmail",
                tool,
                running = self.manager.running_task_count(),
                tracked = tracked.len(),
                "slow call stays a request: task admission is full"
            );
            return Err(operation);
        }
        let run = Arc::clone(run);
        let progress = Arc::clone(&self.progress);
        let options = TaskOptions::new()
            .with_ttl_ms(duration_ms(self.config.ttl))
            .with_poll_interval_ms(duration_ms(self.config.poll_interval))
            .with_status_message(format!("Running {tool}"));
        let task = self.manager.spawn(options, move |context| {
            // From here on the call is a task: progress becomes its status.
            let _ = run.task.set(PromotedRun {
                context: context.clone(),
                progress,
            });
            Box::pin(drive_task(operation, operation_ct, run, context))
        });
        tracked.push_back((task.task_id.clone(), Instant::now()));
        tracing::info!(
            target: "agentmail",
            tool,
            task = %task.task_id,
            "slow call became a task"
        );
        Ok(task)
    }

    /// Whether one more task fits, after forgetting the ones rmcp has evicted.
    fn admits(&self, tracked: &mut VecDeque<(String, Instant)>) -> bool {
        if self.lifetime.is_cancelled() {
            return false;
        }
        // rmcp keeps a finished task for one TTL after it ends, so a task
        // younger than one TTL cannot have been evicted yet.
        tracked.retain(|(task_id, created)| {
            let held =
                created.elapsed() < self.config.ttl || self.manager.get_task(task_id).is_ok();
            if !held {
                self.progress.forget(task_id);
            }
            held
        });
        self.manager.running_task_count() < self.config.max_active
            && tracked.len() < self.config.max_tracked
    }

    /// `tasks/get`, with the latest progress under [`PROGRESS_META_KEY`].
    pub(super) fn get(&self, task_id: &str) -> Result<GetTaskResult, McpError> {
        let mut result = GetTaskResult::new(self.manager.get_task(task_id)?);
        if let Some((progress, total)) = self.progress.get(task_id) {
            let mut meta = MetaObject::default();
            meta.0.insert(
                PROGRESS_META_KEY.to_string(),
                serde_json::json!({ "progress": progress, "total": total }),
            );
            result.meta = Some(meta);
        }
        Ok(result)
    }

    /// `tasks/update`. No tool here asks for input mid-task, so this only
    /// validates the task id; rmcp ignores responses nothing is waiting for.
    pub(super) fn update(&self, task_id: &str, responses: InputResponses) -> Result<(), McpError> {
        self.manager.update_task(task_id, responses)
    }

    /// `tasks/cancel`: records the request; [`drive_task`] passes it on.
    pub(super) fn cancel(&self, task_id: &str) -> Result<(), McpError> {
        self.manager.cancel_task(task_id)
    }

    /// The connection is gone, and its tasks with it: nothing can poll them
    /// any more. Ask every operation to stop, give them
    /// [`TaskConfig::shutdown_grace`] to do so, then abort what is left.
    pub(super) async fn shutdown(&self) {
        self.lifetime.cancel();
        let deadline = tokio::time::Instant::now() + self.config.shutdown_grace;
        while self.manager.running_task_count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.manager.shutdown();
    }
}

/// The body of a task: wait for the operation, passing `tasks/cancel` on to it.
async fn drive_task(
    mut operation: Operation,
    operation_ct: CancellationToken,
    run: Arc<ToolRun>,
    context: TaskContext,
) -> Result<CallToolResult, TaskExit> {
    let joined = tokio::select! {
        biased;
        joined = operation.join() => joined,
        () = context.cancelled() => {
            operation_ct.cancel();
            operation.join().await
        }
    };
    settle_task(joined, &run, &context)
}

/// A task's terminal status. An operation that stopped because it was asked
/// to is `cancelled`, whatever it made of the cancellation (most tools report
/// it as an `isError` result). An `isError` result is otherwise `completed`:
/// SEP-2663 reserves `failed` for a call that produced no result.
fn settle_task(
    joined: Result<Result<CallToolResponse, McpError>, JoinError>,
    run: &ToolRun,
    context: &TaskContext,
) -> Result<CallToolResult, TaskExit> {
    if context.is_cancel_requested() && run.cancel_observed() {
        return Err(TaskExit::Cancelled);
    }
    match joined {
        Ok(Ok(CallToolResponse::Complete(result))) => Ok(result),
        Ok(Ok(_)) => Err(TaskExit::Error(McpError::internal_error(
            "the tool answered with something other than a result, which a task cannot carry",
            None,
        ))),
        Ok(Err(error)) => Err(TaskExit::Error(error)),
        Err(join_error) => Err(TaskExit::Error(operation_lost(&join_error))),
    }
}

/// A call's answer while it is still a request.
fn settle_request(
    joined: Result<Result<CallToolResponse, McpError>, JoinError>,
) -> Result<CallToolResponse, McpError> {
    joined.unwrap_or_else(|join_error| Err(operation_lost(&join_error)))
}

fn operation_lost(join_error: &JoinError) -> McpError {
    if join_error.is_panic() {
        McpError::internal_error("the tool panicked", None)
    } else {
        McpError::internal_error("the tool was stopped before it finished", None)
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ContentBlock, TaskStatus};

    fn quick() -> TaskConfig {
        TaskConfig {
            promote_after: Duration::from_millis(20),
            ..TaskConfig::DEFAULT
        }
    }

    fn text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect()
    }

    fn done(message: &str) -> Result<CallToolResponse, McpError> {
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(message.to_string()),
        ])))
    }

    async fn settled(runtime: &TaskRuntime, task_id: &str) -> GetTaskResult {
        for _ in 0..200 {
            let result = runtime.get(task_id).expect("task is held");
            if result.task.status().is_terminal() {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {task_id} never settled");
    }

    fn task_id(response: Result<CallToolResponse, McpError>) -> String {
        match response {
            Ok(CallToolResponse::Task(created)) => created.task.task_id,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    /// A call that finishes inside the budget is answered inline and never
    /// becomes a task.
    #[tokio::test]
    async fn a_quick_call_stays_a_request() {
        let runtime = TaskRuntime::new(quick());
        let response = runtime
            .run_call(
                "top_senders",
                CancellationToken::new(),
                runtime.operation_token(),
                async { done("quick") },
            )
            .await;
        match response {
            Ok(CallToolResponse::Complete(result)) => assert_eq!(text(&result), "quick"),
            other => panic!("expected an inline result, got {other:?}"),
        }
        assert!(runtime.tracked.lock().is_empty());
    }

    /// The regression the token rule exists for: rmcp cancels the request's
    /// token as it sends the `CreateTaskResult`, and the task must carry on
    /// regardless and complete with the operation's own result.
    #[tokio::test]
    async fn a_promoted_task_outlives_its_request_token() {
        let runtime = TaskRuntime::new(quick());
        let request_ct = CancellationToken::new();
        let operation_ct = runtime.operation_token();
        let seen_by_operation = operation_ct.clone();
        let response = runtime
            .run_call(
                "top_senders",
                request_ct.clone(),
                operation_ct,
                async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    if seen_by_operation.is_cancelled() {
                        return done("cancelled");
                    }
                    done("finished")
                },
            )
            .await;
        let task_id = task_id(response);
        // What rmcp does when it sends the response.
        request_ct.cancel();
        let result = settled(&runtime, &task_id).await;
        assert_eq!(result.task.status(), TaskStatus::Completed, "{result:?}");
        let payload = serde_json::to_value(&result).unwrap();
        assert_eq!(
            payload["result"]["content"][0]["text"], "finished",
            "{payload:#}"
        );
    }

    /// `notifications/cancelled` before promotion stops the operation at its
    /// next check, the way a plain call stops, and nothing becomes a task.
    #[tokio::test]
    async fn a_request_cancelled_before_promotion_stops_its_operation() {
        let runtime = TaskRuntime::new(TaskConfig {
            promote_after: Duration::from_secs(60),
            ..TaskConfig::DEFAULT
        });
        let request_ct = CancellationToken::new();
        let operation_ct = runtime.operation_token();
        let watched = operation_ct.clone();
        let cancel = request_ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.cancel();
        });
        let response = runtime
            .run_call("top_senders", request_ct, operation_ct, async move {
                watched.cancelled().await;
                done("stopped")
            })
            .await;
        match response {
            Ok(CallToolResponse::Complete(result)) => assert_eq!(text(&result), "stopped"),
            other => panic!("expected the operation's own answer, got {other:?}"),
        }
        assert!(runtime.tracked.lock().is_empty());
    }

    /// `tasks/cancel` reaches the operation through its token; one that saw
    /// the cancellation settles as `cancelled`, even though the tool reported
    /// it as an ordinary (`isError`) result.
    #[tokio::test]
    async fn tasks_cancel_settles_an_operation_that_saw_it_as_cancelled() {
        let runtime = TaskRuntime::new(quick());
        let operation_ct = runtime.operation_token();
        let watched = operation_ct.clone();
        let response = runtime
            .run_call(
                "delete_by_sender",
                CancellationToken::new(),
                operation_ct,
                async move {
                    watched.cancelled().await;
                    // What `make_cancel_fn` records when the library checks it.
                    if let Some(run) = ToolRun::current() {
                        run.note_cancel_observed();
                    }
                    Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                        ContentBlock::text("Operation cancelled".to_string()),
                    ])))
                },
            )
            .await;
        let task_id = task_id(response);
        runtime.cancel(&task_id).unwrap();
        let result = settled(&runtime, &task_id).await;
        assert_eq!(result.task.status(), TaskStatus::Cancelled, "{result:?}");
    }

    /// A cancel that arrives after the work is done changes nothing: the task
    /// reports what the operation did.
    #[tokio::test]
    async fn an_operation_that_finishes_despite_a_cancel_completes() {
        let runtime = TaskRuntime::new(quick());
        let response = runtime
            .run_call(
                "delete_by_sender",
                CancellationToken::new(),
                runtime.operation_token(),
                async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    done("all deleted")
                },
            )
            .await;
        let task_id = task_id(response);
        runtime.cancel(&task_id).unwrap();
        let result = settled(&runtime, &task_id).await;
        assert_eq!(result.task.status(), TaskStatus::Completed, "{result:?}");
    }

    /// A tool that panics after promotion fails its task instead of leaving it
    /// `working` until the TTL (rmcp's own completion path never runs for a
    /// future that unwinds).
    #[tokio::test]
    async fn a_panicking_operation_fails_its_task() {
        let runtime = TaskRuntime::new(quick());
        let response = runtime
            .run_call(
                "top_domains",
                CancellationToken::new(),
                runtime.operation_token(),
                async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    panic!("boom");
                },
            )
            .await;
        let task_id = task_id(response);
        let result = settled(&runtime, &task_id).await;
        assert_eq!(result.task.status(), TaskStatus::Failed, "{result:?}");
    }

    /// A panic while the call is still a request is an internal error, not a
    /// request that never gets an answer.
    #[tokio::test]
    async fn a_panic_before_promotion_is_an_error_response() {
        let runtime = TaskRuntime::new(TaskConfig {
            promote_after: Duration::from_secs(60),
            ..TaskConfig::DEFAULT
        });
        let response = runtime
            .run_call(
                "top_domains",
                CancellationToken::new(),
                runtime.operation_token(),
                async {
                    panic!("boom");
                },
            )
            .await;
        let error = response.expect_err("a panic is an error");
        assert!(error.message.contains("panicked"), "{error:?}");
    }

    /// rmcp's TTL is the hard stop: a task still running past it is aborted
    /// and fails, and its operation does not run on.
    #[tokio::test]
    async fn the_ttl_aborts_a_task_that_runs_past_it() {
        let runtime = TaskRuntime::new(TaskConfig {
            ttl: Duration::from_millis(150),
            ..quick()
        });
        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);
        let response = runtime
            .run_call(
                "top_senders",
                CancellationToken::new(),
                runtime.operation_token(),
                async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    flag.store(true, Ordering::SeqCst);
                    done("too late")
                },
            )
            .await;
        let task_id = task_id(response);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let result = settled(&runtime, &task_id).await;
        assert_eq!(result.task.status(), TaskStatus::Failed, "{result:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !finished.load(Ordering::SeqCst),
            "the aborted operation must not run on"
        );
    }

    /// Progress after promotion lands on the task: its status message and the
    /// numbers `tasks/get` reports under `io.agentmuse/progress`.
    #[tokio::test]
    async fn progress_after_promotion_is_the_tasks_status() {
        let runtime = TaskRuntime::new(quick());
        let response = runtime
            .run_call(
                "top_senders",
                CancellationToken::new(),
                runtime.operation_token(),
                async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    let run = ToolRun::current().expect("running inside a promotable call");
                    assert!(run.is_promoted());
                    assert!(run.report_progress(3, 10));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    done("ranked")
                },
            )
            .await;
        let task_id = task_id(response);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let running = runtime.get(&task_id).unwrap();
        assert_eq!(running.task.task.status_message.as_deref(), Some("3 of 10"));
        let meta = running.meta.expect("progress meta");
        assert_eq!(
            meta.0.get(PROGRESS_META_KEY),
            Some(&serde_json::json!({ "progress": 3, "total": 10 }))
        );
    }

    /// Admission is a ceiling, not a refusal: a call past it simply finishes
    /// as a request.
    #[tokio::test]
    async fn a_call_past_admission_finishes_as_a_request() {
        let runtime = TaskRuntime::new(TaskConfig {
            max_active: 0,
            ..quick()
        });
        let response = runtime
            .run_call(
                "top_senders",
                CancellationToken::new(),
                runtime.operation_token(),
                async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    done("inline after all")
                },
            )
            .await;
        match response {
            Ok(CallToolResponse::Complete(result)) => assert_eq!(text(&result), "inline after all"),
            other => panic!("expected an inline result, got {other:?}"),
        }
    }

    /// Closing the connection cancels every operation token, and what does
    /// not stop within the grace period is aborted.
    #[tokio::test]
    async fn shutdown_stops_running_tasks() {
        let runtime = TaskRuntime::new(TaskConfig {
            shutdown_grace: Duration::from_millis(100),
            ..quick()
        });
        let operation_ct = runtime.operation_token();
        let watched = operation_ct.clone();
        let response = runtime
            .run_call(
                "top_senders",
                CancellationToken::new(),
                operation_ct,
                async move {
                    watched.cancelled().await;
                    done("stopped for shutdown")
                },
            )
            .await;
        let task_id = task_id(response);
        runtime.shutdown().await;
        assert_eq!(runtime.manager.running_task_count(), 0);
        assert!(runtime.get(&task_id).is_err(), "shutdown drops every task");
    }
}
