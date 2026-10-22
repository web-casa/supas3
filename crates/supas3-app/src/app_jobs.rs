// SPDX-License-Identifier: AGPL-3.0-or-later
//! Work the application runs itself, outside the storage backend: copying on
//! this computer, moving, renaming folders. Each job runs on its own thread
//! and reports through a shared table the task center reads, the same way it
//! reads transfer events. Jobs do not survive a restart.

use crate::tasks::{Task, TaskKind, TaskOrigin, TaskState};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct AppJobs {
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    /// The latest state of each job that changed since the last collection.
    changed: HashMap<String, Task>,
    /// How to ask a running job to stop.
    cancels: HashMap<String, Arc<AtomicBool>>,
}

/// What a job is, for the task center's row.
#[derive(Debug, Clone)]
pub struct JobLabel {
    pub kind: TaskKind,
    pub name: String,
    pub source: String,
    pub target: String,
}

/// What a running job sees: where to report progress, and whether it was
/// asked to stop.
pub struct JobContext {
    id: String,
    label: JobLabel,
    created_at: i64,
    cancel: Arc<AtomicBool>,
    shared: Arc<Mutex<Shared>>,
}

impl JobContext {
    /// Whether the user cancelled. A job checks this between steps and
    /// returns `Err(JobError::Cancelled)` when it is set.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Reports how much is done, in whatever unit `total` uses.
    pub fn progress(&self, processed: u64, total: u64) {
        self.report(TaskState::Running, processed, total);
    }

    fn report(&self, state: TaskState, processed: u64, total: u64) {
        let task = Task {
            id: self.id.clone(),
            kind: self.label.kind,
            name: self.label.name.clone(),
            source: self.label.source.clone(),
            target: self.label.target.clone(),
            state,
            processed,
            total,
            bytes_per_second: 0.0,
            group: None,
            group_name: None,
            retryable: false,
            origin: TaskOrigin::App,
            created_at: self.created_at,
        };
        let mut shared = lock(&self.shared);
        // An end is never replaced by a later progress report.
        let ended = shared
            .changed
            .get(&self.id)
            .is_some_and(|held| !held.state.is_active());
        if !ended {
            shared.changed.insert(self.id.clone(), task);
        }
    }
}

/// How a job ended without completing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    Cancelled,
    Failed(String),
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl AppJobs {
    /// Starts `work` on its own thread and returns the job's id. `total` is
    /// the size of the work as first known; the job may report a better one.
    pub fn start(
        &self,
        label: JobLabel,
        total: u64,
        work: impl FnOnce(&JobContext) -> Result<(), JobError> + Send + 'static,
    ) -> String {
        let id = format!("app-{}", uuid_like());
        let cancel = Arc::new(AtomicBool::new(false));
        lock(&self.shared)
            .cancels
            .insert(id.clone(), cancel.clone());
        let context = JobContext {
            id: id.clone(),
            label,
            created_at: now_millis(),
            cancel,
            shared: self.shared.clone(),
        };
        context.report(TaskState::Queued, 0, total);
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("supas3-job".into())
            .spawn(move || {
                context.progress(0, total);
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&context)));
                let state = match outcome {
                    Ok(Ok(())) => TaskState::Completed,
                    Ok(Err(JobError::Cancelled)) => TaskState::Cancelled,
                    Ok(Err(JobError::Failed(message))) => TaskState::Failed(message),
                    Err(_) => TaskState::Failed("The job stopped unexpectedly.".into()),
                };
                let (processed, total) = {
                    let shared = lock(&shared);
                    shared
                        .changed
                        .get(&context.id)
                        .map_or((0, total), |task| (task.processed, task.total))
                };
                context.report(state, processed, total);
                lock(&shared).cancels.remove(&context.id);
            })
            .expect("a thread can be started");
        id
    }

    /// Asks a job to stop. It ends at its next check, as cancelled.
    pub fn cancel(&self, id: &str) {
        if let Some(cancel) = lock(&self.shared).cancels.get(id) {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// The jobs that changed since the last call.
    pub fn collect(&self) -> Vec<Task> {
        lock(&self.shared)
            .changed
            .drain()
            .map(|(_, task)| task)
            .collect()
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// A unique id without another dependency: time plus a counter.
fn uuid_like() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}-{:x}",
        now_millis(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label() -> JobLabel {
        JobLabel {
            kind: TaskKind::LocalCopy,
            name: "a".into(),
            source: "/from".into(),
            target: "/to".into(),
        }
    }

    /// Collects until the job has ended, and returns its last state.
    fn wait(jobs: &AppJobs, id: &str) -> Task {
        for _ in 0..200 {
            if let Some(task) = jobs
                .collect()
                .into_iter()
                .find(|task| task.id == id && !task.state.is_active())
            {
                return task;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the job did not end");
    }

    #[test]
    fn a_job_reports_progress_and_how_it_ended() {
        let jobs = AppJobs::default();
        let id = jobs.start(label(), 10, |job| {
            job.progress(10, 10);
            Ok(())
        });
        let task = wait(&jobs, &id);
        assert_eq!(task.state, TaskState::Completed);
        assert_eq!((task.processed, task.total), (10, 10));
        assert_eq!(task.origin, TaskOrigin::App);

        let failed = jobs.start(label(), 1, |_| Err(JobError::Failed("disk full".into())));
        assert_eq!(
            wait(&jobs, &failed).state,
            TaskState::Failed("disk full".into())
        );

        let panicked = jobs.start(label(), 1, |_| panic!("bug"));
        assert!(matches!(wait(&jobs, &panicked).state, TaskState::Failed(_)));
    }

    #[test]
    fn a_cancelled_job_ends_as_cancelled() {
        let jobs = AppJobs::default();
        let id = jobs.start(label(), 1, |job| {
            for _ in 0..500 {
                if job.cancelled() {
                    return Err(JobError::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(())
        });
        jobs.cancel(&id);
        assert_eq!(wait(&jobs, &id).state, TaskState::Cancelled);
    }
}
