// SPDX-License-Identifier: AGPL-3.0-or-later
//! The transfers the task center shows, kept up to date from backend events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use supas3_core::host::{Event, EventSink};
use supas3_core::transfer::{TransferEvent, TransferJob, TransferStatus, TransferType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Upload,
    Download,
    /// Storage to storage, relayed through a temporary file on this computer.
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Running,
    Paused,
    Completed,
    Failed(String),
    Cancelled,
}

impl TaskState {
    fn of(status: &TransferStatus) -> Self {
        match status {
            TransferStatus::Pending => Self::Queued,
            TransferStatus::InProgress => Self::Running,
            TransferStatus::Paused => Self::Paused,
            TransferStatus::Completed => Self::Completed,
            TransferStatus::Failed(message) => Self::Failed(message.clone()),
            TransferStatus::Cancelled => Self::Cancelled,
        }
    }

    /// Not finished yet.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Paused)
    }

    /// Waiting or running: the backend can cancel it.
    pub fn can_cancel(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// Stopped without finishing.
    pub fn has_stopped_short(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::Cancelled)
    }

    /// Completed or cancelled: nothing more to do or to look into.
    pub fn is_settled(&self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

/// One transfer as the task center shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: String,
    pub kind: TaskKind,
    /// The file name, for the row's title.
    pub name: String,
    /// Where the data comes from and goes to, fixed when the task was queued.
    pub source: String,
    pub target: String,
    pub state: TaskState,
    pub processed: u64,
    pub total: u64,
    pub bytes_per_second: f64,
    /// The batch this task was queued in, if any.
    pub group: Option<String>,
    pub group_name: Option<String>,
    /// Whether the backend can queue it again. A transfer restored from an
    /// earlier run has lost what it needs for that (where a download was
    /// going, or the source of a copy), so it has to be started afresh.
    pub retryable: bool,
    created_at: i64,
}

impl Task {
    /// `restored`: the job comes from the backend's own list rather than
    /// from an event of this run, so its session-only fields are meaningful.
    fn from_job(job: &TransferJob, restored: bool) -> Self {
        let remote = format!("s3://{}/{}", job.bucket, job.key);
        let (kind, source, target) = match (&job.remote_source, &job.transfer_type) {
            (Some(origin), _) => (
                TaskKind::Copy,
                format!("s3://{}/{}", origin.bucket, origin.key),
                remote,
            ),
            (None, TransferType::Upload) => (TaskKind::Upload, job.local_path.clone(), remote),
            (None, TransferType::Download) => (TaskKind::Download, remote, job.local_path.clone()),
        };
        let name = job
            .key
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&job.key)
            .to_string();
        Self {
            id: job.id.clone(),
            kind,
            name,
            source,
            target,
            state: TaskState::of(&job.status),
            processed: job.processed_bytes,
            total: job.total_bytes,
            bytes_per_second: job.bytes_per_second,
            group: job.parent_group_id.clone(),
            group_name: job.group_name.clone(),
            retryable: !restored || can_be_retried(job),
            created_at: job.created_at,
        }
    }

    /// Progress from 0 to 1, or `None` while the size is unknown.
    pub fn fraction(&self) -> Option<f32> {
        match self.state {
            TaskState::Completed => Some(1.0),
            _ if self.total == 0 => None,
            _ => Some((self.processed as f64 / self.total as f64).clamp(0.0, 1.0) as f32),
        }
    }
}

/// Whether the backend accepts a retry of a job read from its own list. These
/// mirror the checks in its retry command.
fn can_be_retried(job: &TransferJob) -> bool {
    let copy_lost_its_source = job
        .remote_source
        .as_ref()
        .is_some_and(|source| !source.current_session);
    let sync_lost_its_plan = job
        .sync_source
        .as_ref()
        .is_some_and(|source| !source.current_session);
    let import_lost_its_url = job
        .url_import
        .as_ref()
        .is_some_and(|source| source.url.is_empty());
    let download_lost_its_destination =
        matches!(job.transfer_type, TransferType::Download) && job.download_destination.is_none();
    !(copy_lost_its_source
        || sync_lost_its_plan
        || import_lost_its_url
        || download_lost_its_destination)
}

/// Which tasks a view of the task center lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskFilter {
    /// Waiting, running and failed: everything that may need attention.
    #[default]
    Current,
    Failed,
    Finished,
}

/// How many tasks are in each condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskCounts {
    pub active: usize,
    pub failed: usize,
    pub finished: usize,
}

/// How many early updates and removals are remembered at most.
const MEMORY_LIMIT: usize = 4096;

/// Every transfer the backend knows about.
///
/// Events can arrive out of order: the backend may report progress, even
/// completion, before the event that announces a transfer, and a slow
/// snapshot can describe a state older than events already applied. The rules
/// here keep a task from going backwards: once it has ended it stays ended.
#[derive(Debug, Default)]
pub struct Tasks {
    by_id: HashMap<String, Task>,
    /// Updates for transfers that have not been announced yet.
    early: HashMap<String, TransferEvent>,
    /// Transfers the user removed, so a late announcement does not bring
    /// them back.
    removed: std::collections::HashSet<String>,
}

impl Tasks {
    /// Brings the list in line with the backend's own list. Transfers the
    /// backend no longer has are dropped; a task that has already ended here
    /// is not put back into an earlier state by an older snapshot.
    pub fn merge_snapshot(&mut self, jobs: &[TransferJob]) {
        let mut merged = HashMap::with_capacity(jobs.len());
        for job in jobs {
            if self.removed.contains(&job.id) {
                continue;
            }
            let mut task = Task::from_job(job, true);
            if let Some(known) = self.by_id.get(&job.id) {
                if !known.state.is_active() && task.state.is_active() {
                    task.state = known.state.clone();
                    task.processed = known.processed;
                }
            }
            merged.insert(job.id.clone(), task);
        }
        self.by_id = merged;
        self.apply_early();
    }

    /// Records a newly queued transfer. Returns whether it ended up finished,
    /// which happens when its completion was reported before it was announced.
    pub fn add(&mut self, job: &TransferJob) -> bool {
        if self.removed.contains(&job.id) {
            return false;
        }
        let announced = Task::from_job(job, false);
        match self.by_id.get(&job.id) {
            // Already known and further along: the announcement is old news.
            Some(known) if !known.state.is_active() => {}
            _ => {
                self.by_id.insert(job.id.clone(), announced);
            }
        }
        let was_active = self.by_id[&job.id].state.is_active();
        if let Some(update) = self.early.remove(&job.id) {
            self.update(&update);
        }
        was_active && self.by_id[&job.id].state == TaskState::Completed
    }

    fn apply_early(&mut self) {
        let ready: Vec<String> = self
            .early
            .keys()
            .filter(|id| self.by_id.contains_key(*id))
            .cloned()
            .collect();
        for id in ready {
            if let Some(update) = self.early.remove(&id) {
                self.update(&update);
            }
        }
    }

    /// Applies a progress or status change. Returns whether a known task
    /// changed. An update for a transfer that has not been announced yet is
    /// kept for when it is.
    pub fn update(&mut self, event: &TransferEvent) -> bool {
        let Some(task) = self.by_id.get_mut(&event.job_id) else {
            if !self.removed.contains(&event.job_id) && self.early.len() < MEMORY_LIMIT {
                let keep = self
                    .early
                    .get(&event.job_id)
                    .is_none_or(|held| supersedes(event, held));
                if keep {
                    self.early.insert(event.job_id.clone(), event.clone());
                }
            }
            return false;
        };
        let state = TaskState::of(&event.status);
        // A late progress report must not revive a task that has ended.
        if !task.state.is_active() && state.is_active() {
            return false;
        }
        task.state = state;
        task.processed = event.processed_bytes;
        if event.total_bytes > 0 {
            task.total = event.total_bytes;
        }
        task.bytes_per_second = event.bytes_per_second;
        true
    }

    /// Forgets a transfer the user removed.
    pub fn remove(&mut self, id: &str) {
        self.by_id.remove(id);
        self.early.remove(id);
        if self.removed.len() >= MEMORY_LIMIT {
            self.removed.clear();
        }
        self.removed.insert(id.to_string());
    }

    /// The ids of every completed or cancelled transfer. Failed ones are not
    /// included: they are kept until the user has dealt with them.
    pub fn settled_ids(&self) -> Vec<String> {
        self.by_id
            .values()
            .filter(|task| task.state.is_settled())
            .map(|task| task.id.clone())
            .collect()
    }

    /// The ids of every transfer that can be cancelled.
    pub fn cancellable_ids(&self) -> Vec<String> {
        self.by_id
            .values()
            .filter(|task| task.state.can_cancel())
            .map(|task| task.id.clone())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.by_id.get(id)
    }

    pub fn counts(&self) -> TaskCounts {
        let mut counts = TaskCounts::default();
        for task in self.by_id.values() {
            match task.state {
                TaskState::Failed(_) => counts.failed += 1,
                TaskState::Completed | TaskState::Cancelled => counts.finished += 1,
                _ => counts.active += 1,
            }
        }
        counts
    }

    /// The tasks a filter lists, newest first.
    pub fn list(&self, filter: TaskFilter) -> Vec<&Task> {
        let mut tasks: Vec<&Task> = self
            .by_id
            .values()
            .filter(|task| match filter {
                TaskFilter::Current => {
                    task.state.is_active() || matches!(task.state, TaskState::Failed(_))
                }
                TaskFilter::Failed => matches!(task.state, TaskState::Failed(_)),
                TaskFilter::Finished => {
                    matches!(task.state, TaskState::Completed | TaskState::Cancelled)
                }
            })
            .collect();
        tasks.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        tasks
    }

    /// Combined progress of the active tasks: bytes done, bytes in total and
    /// current speed.
    pub fn active_progress(&self) -> (u64, u64, f64) {
        self.by_id
            .values()
            .filter(|task| task.state.is_active())
            .fold((0, 0, 0.0), |(done, total, speed), task| {
                (
                    done + task.processed,
                    total + task.total,
                    speed + task.bytes_per_second,
                )
            })
    }
}

/// Whether `new` should replace `held` as the latest word on one transfer: a
/// report that the transfer ended is never replaced by one that it is still
/// going, whichever arrives last.
fn supersedes(new: &TransferEvent, held: &TransferEvent) -> bool {
    let ended = |event: &TransferEvent| !TaskState::of(&event.status).is_active();
    ended(new) || !ended(held)
}

/// The most announcements held before the interface is told to re-read the
/// backend's list instead.
const ADDED_LIMIT: usize = 5000;

/// Backend events waiting to be shown.
///
/// The backend calls the sink from transfer tasks, sometimes while holding its
/// own locks, so the sink only stores the event and returns. Memory stays
/// bounded however long the interface takes to collect: progress reports
/// replace each other per task, so at most one is kept for each, and past a
/// limit announcements are dropped in favour of re-reading the backend's list.
#[derive(Default)]
pub struct EventQueue {
    pending: Mutex<Pending>,
}

#[derive(Default)]
struct Pending {
    added: Vec<TransferJob>,
    updates: HashMap<String, TransferEvent>,
    overflowed: bool,
}

/// What the interface collects from the queue in one go.
#[derive(Default)]
pub struct Collected {
    pub added: Vec<TransferJob>,
    pub updates: Vec<TransferEvent>,
    /// Announcements were dropped: read the backend's list to catch up.
    pub overflowed: bool,
}

impl Collected {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updates.is_empty() && !self.overflowed
    }
}

impl EventQueue {
    /// The sink to hand to the backend.
    pub fn sink(self: &Arc<Self>) -> EventSink {
        let queue = self.clone();
        Arc::new(move |event| queue.push(event))
    }

    fn push(&self, event: Event) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match event.name.as_str() {
            "transfer-added" => {
                if pending.added.len() >= ADDED_LIMIT {
                    pending.overflowed = true;
                } else if let Ok(job) = serde_json::from_value::<TransferJob>(event.payload) {
                    pending.added.push(job);
                }
            }
            "transfer-update" => {
                if let Ok(update) = serde_json::from_value::<TransferEvent>(event.payload) {
                    let keep = pending
                        .updates
                        .get(&update.job_id)
                        .is_none_or(|held| supersedes(&update, held));
                    if keep {
                        pending.updates.insert(update.job_id.clone(), update);
                    }
                }
            }
            _ => {}
        }
    }

    /// Takes everything that has arrived since the last call.
    pub fn collect(&self) -> Collected {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Collected {
            added: std::mem::take(&mut pending.added),
            updates: pending.updates.drain().map(|(_, update)| update).collect(),
            overflowed: std::mem::take(&mut pending.overflowed),
        }
    }
}

/// A speed as people read it, such as `1.2 MB/s`.
pub fn format_speed(bytes_per_second: f64) -> String {
    format!(
        "{}/s",
        crate::listing::format_size(bytes_per_second.max(0.0) as u64)
    )
}

#[cfg(test)]
mod tests {
    use super::{format_speed, EventQueue, TaskFilter, TaskKind, TaskState, Tasks};
    use std::path::PathBuf;
    use std::sync::Arc;
    use supas3_core::host::Event;
    use supas3_core::transfer::{TransferEvent, TransferJob, TransferStatus, TransferType};

    fn job(id: &str, kind: TransferType, key: &str, created_at: i64) -> TransferJob {
        let mut job = TransferJob::new(
            kind,
            "profile".into(),
            "bucket".into(),
            None,
            key.into(),
            PathBuf::from("/home/user/file.bin"),
            100,
        );
        job.id = id.into();
        job.created_at = created_at;
        job
    }

    fn update(id: &str, status: TransferStatus, processed: u64) -> TransferEvent {
        TransferEvent {
            phase: None,
            job_id: id.into(),
            bytes_per_second: 10.0,
            processed_bytes: processed,
            total_bytes: 100,
            status,
            finished_at: None,
        }
    }

    #[test]
    fn a_task_names_its_fixed_source_and_target() {
        let mut tasks = Tasks::default();
        tasks.add(&job("u", TransferType::Upload, "照片/海边.jpg", 1));
        tasks.add(&job("d", TransferType::Download, "docs/a.txt", 2));

        let upload = tasks.get("u").unwrap();
        assert_eq!(upload.kind, TaskKind::Upload);
        assert_eq!(upload.name, "海边.jpg");
        assert_eq!(upload.source, "/home/user/file.bin");
        assert_eq!(upload.target, "s3://bucket/照片/海边.jpg");

        let download = tasks.get("d").unwrap();
        assert_eq!(download.kind, TaskKind::Download);
        assert_eq!(download.source, "s3://bucket/docs/a.txt");
        assert_eq!(download.target, "/home/user/file.bin");
    }

    #[test]
    fn updates_move_a_task_through_its_states() {
        let mut tasks = Tasks::default();
        tasks.add(&job("a", TransferType::Upload, "a", 1));
        assert_eq!(tasks.get("a").unwrap().state, TaskState::Queued);

        assert!(tasks.update(&update("a", TransferStatus::InProgress, 40)));
        let task = tasks.get("a").unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(task.fraction(), Some(0.4));

        assert!(tasks.update(&update("a", TransferStatus::Failed("denied".into()), 40)));
        assert_eq!(
            tasks.get("a").unwrap().state,
            TaskState::Failed("denied".into())
        );
        assert!(tasks.get("a").unwrap().state.has_stopped_short());
        assert!(tasks.get("a").unwrap().retryable);
    }

    #[test]
    fn a_late_progress_report_does_not_revive_a_finished_task() {
        let mut tasks = Tasks::default();
        tasks.add(&job("a", TransferType::Upload, "a", 1));
        tasks.update(&update("a", TransferStatus::Completed, 100));

        assert!(!tasks.update(&update("a", TransferStatus::InProgress, 90)));
        assert_eq!(tasks.get("a").unwrap().state, TaskState::Completed);
        assert_eq!(tasks.get("a").unwrap().fraction(), Some(1.0));
    }

    #[test]
    fn a_removed_task_stays_removed() {
        let mut tasks = Tasks::default();
        tasks.add(&job("a", TransferType::Upload, "a", 1));
        tasks.remove("a");
        assert!(!tasks.update(&update("a", TransferStatus::Completed, 100)));
        // A late announcement or an older snapshot must not bring it back.
        assert!(!tasks.add(&job("a", TransferType::Upload, "a", 1)));
        tasks.merge_snapshot(&[job("a", TransferType::Upload, "a", 1)]);
        assert!(tasks.get("a").is_none());
    }

    #[test]
    fn a_result_reported_before_the_announcement_is_not_lost() {
        // The backend can finish a transfer before the interface has heard
        // that it exists.
        let mut tasks = Tasks::default();
        assert!(!tasks.update(&update("a", TransferStatus::Failed("denied".into()), 0)));
        assert!(tasks.get("a").is_none());

        let finished = tasks.add(&job("a", TransferType::Upload, "a", 1));
        assert!(!finished);
        assert_eq!(
            tasks.get("a").unwrap().state,
            TaskState::Failed("denied".into())
        );

        assert!(!tasks.update(&update("b", TransferStatus::Completed, 100)));
        assert!(
            tasks.add(&job("b", TransferType::Upload, "b", 2)),
            "completion is reported once"
        );
        assert_eq!(tasks.get("b").unwrap().state, TaskState::Completed);
    }

    #[test]
    fn a_late_waiting_report_does_not_replace_an_early_result() {
        let mut tasks = Tasks::default();
        tasks.update(&update("a", TransferStatus::Completed, 100));
        tasks.update(&update("a", TransferStatus::Pending, 0));
        tasks.add(&job("a", TransferType::Upload, "a", 1));
        assert_eq!(tasks.get("a").unwrap().state, TaskState::Completed);

        // The same order inside the queue, before the interface collects.
        let queue = Arc::new(EventQueue::default());
        let sink = queue.sink();
        for status in [TransferStatus::Completed, TransferStatus::Pending] {
            sink(Event {
                name: "transfer-update".into(),
                payload: serde_json::to_value(update("q", status, 100)).unwrap(),
            });
        }
        let collected = queue.collect();
        assert_eq!(collected.updates.len(), 1);
        assert_eq!(collected.updates[0].status, TransferStatus::Completed);
    }

    #[test]
    fn an_announcement_does_not_reset_a_task_that_has_ended() {
        let mut tasks = Tasks::default();
        tasks.add(&job("a", TransferType::Upload, "a", 1));
        tasks.update(&update("a", TransferStatus::Completed, 100));
        tasks.add(&job("a", TransferType::Upload, "a", 1));
        assert_eq!(tasks.get("a").unwrap().state, TaskState::Completed);
    }

    #[test]
    fn a_snapshot_updates_membership_without_undoing_results() {
        let mut tasks = Tasks::default();
        tasks.add(&job("done", TransferType::Upload, "done", 1));
        tasks.add(&job("gone", TransferType::Upload, "gone", 2));
        tasks.update(&update("done", TransferStatus::Completed, 100));

        // A snapshot taken before "done" completed, after "gone" was cleared.
        tasks.merge_snapshot(&[
            job("done", TransferType::Upload, "done", 1),
            job("new", TransferType::Upload, "new", 3),
        ]);
        assert_eq!(tasks.get("done").unwrap().state, TaskState::Completed);
        assert!(tasks.get("gone").is_none());
        assert_eq!(tasks.get("new").unwrap().state, TaskState::Queued);
    }

    #[test]
    fn a_transfer_restored_without_what_it_needs_cannot_be_retried() {
        let mut tasks = Tasks::default();
        // A download read from the journal has no destination handle.
        let mut download = job("d", TransferType::Download, "d", 1);
        download.status = TransferStatus::Failed("interrupted".into());
        let mut upload = job("u", TransferType::Upload, "u", 2);
        upload.status = TransferStatus::Failed("interrupted".into());
        tasks.merge_snapshot(&[download.clone(), upload]);
        assert!(!tasks.get("d").unwrap().retryable);
        assert!(tasks.get("u").unwrap().retryable);

        // The same download announced in this run can be retried.
        let mut fresh = Tasks::default();
        fresh.add(&download);
        assert!(fresh.get("d").unwrap().retryable);
    }

    #[test]
    fn clearing_and_cancelling_pick_the_right_tasks() {
        let mut tasks = Tasks::default();
        for (id, status) in [
            ("waiting", TransferStatus::Pending),
            ("running", TransferStatus::InProgress),
            ("paused", TransferStatus::Paused),
            ("done", TransferStatus::Completed),
            ("cancelled", TransferStatus::Cancelled),
            ("failed", TransferStatus::Failed("x".into())),
        ] {
            tasks.add(&job(id, TransferType::Upload, id, 1));
            tasks.update(&update(id, status, 0));
        }
        let sorted = |mut ids: Vec<String>| {
            ids.sort();
            ids
        };
        assert_eq!(
            sorted(tasks.settled_ids()),
            ["cancelled", "done"],
            "failures are kept"
        );
        assert_eq!(sorted(tasks.cancellable_ids()), ["running", "waiting"]);
    }

    #[test]
    fn the_queue_reports_an_overflow_instead_of_growing_without_limit() {
        let queue = Arc::new(EventQueue::default());
        let sink = queue.sink();
        let payload = serde_json::to_value(job("a", TransferType::Upload, "a", 1)).unwrap();
        for _ in 0..(super::ADDED_LIMIT + 10) {
            sink(Event {
                name: "transfer-added".into(),
                payload: payload.clone(),
            });
        }
        let collected = queue.collect();
        assert_eq!(collected.added.len(), super::ADDED_LIMIT);
        assert!(collected.overflowed);
        assert!(!queue.collect().overflowed);
    }

    #[test]
    fn filters_and_counts_separate_what_needs_attention() {
        let mut tasks = Tasks::default();
        for (id, at) in [
            ("old", 1),
            ("running", 2),
            ("failed", 3),
            ("done", 4),
            ("cancelled", 5),
        ] {
            tasks.add(&job(id, TransferType::Upload, id, at));
        }
        tasks.update(&update("running", TransferStatus::InProgress, 10));
        tasks.update(&update("failed", TransferStatus::Failed("x".into()), 0));
        tasks.update(&update("done", TransferStatus::Completed, 100));
        tasks.update(&update("cancelled", TransferStatus::Cancelled, 0));

        let ids = |filter| -> Vec<String> {
            tasks
                .list(filter)
                .into_iter()
                .map(|task| task.id.clone())
                .collect()
        };
        assert_eq!(
            ids(TaskFilter::Current),
            ["failed", "running", "old"],
            "newest first"
        );
        assert_eq!(ids(TaskFilter::Failed), ["failed"]);
        assert_eq!(ids(TaskFilter::Finished), ["cancelled", "done"]);

        let counts = tasks.counts();
        assert_eq!((counts.active, counts.failed, counts.finished), (2, 1, 2));
        assert_eq!(tasks.active_progress(), (10, 200, 10.0));
    }

    #[test]
    fn the_queue_keeps_only_the_latest_progress_per_task() {
        let queue = Arc::new(EventQueue::default());
        let sink = queue.sink();
        let send = |name: &str, payload: serde_json::Value| {
            sink(Event {
                name: name.into(),
                payload,
            })
        };
        send(
            "transfer-added",
            serde_json::to_value(job("a", TransferType::Upload, "a", 1)).unwrap(),
        );
        for processed in 0..1000 {
            send(
                "transfer-update",
                serde_json::to_value(update("a", TransferStatus::InProgress, processed)).unwrap(),
            );
        }
        send("something-else", serde_json::json!({}));
        send("transfer-update", serde_json::json!({ "not": "an update" }));

        let collected = queue.collect();
        assert_eq!(collected.added.len(), 1);
        assert_eq!(
            collected.updates.len(),
            1,
            "a thousand reports collapse into one"
        );
        assert_eq!(collected.updates[0].processed_bytes, 999);
        assert!(queue.collect().is_empty());
    }

    #[test]
    fn speeds_are_shown_per_second() {
        assert_eq!(format_speed(0.0), "0 B/s");
        assert_eq!(format_speed(1536.0), "1.5 KB/s");
        assert_eq!(format_speed(-5.0), "0 B/s");
    }
}
