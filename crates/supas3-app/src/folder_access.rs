// SPDX-License-Identifier: AGPL-3.0-or-later
//! Giving every object under a folder the same canned ACL, one object at a
//! time, as an application job the user can cancel.

use crate::app_jobs::{JobContext, JobError};

/// The most objects a folder may hold for this; more is refused up front.
pub const FOLDER_ACCESS_LIMIT: usize = 1_000;

/// What the job needs from storage.
pub trait AccessBackend {
    /// Gives one object the ACL chosen for the job.
    fn set(&self, key: &str) -> Result<(), String>;
    /// The connection's identity now, to notice it being edited meanwhile.
    fn identity(&self) -> Option<String>;
}

/// What the job did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub changed: usize,
    pub failed: usize,
    /// Not reached, after a cancel or a stop.
    pub left: usize,
    pub first_error: Option<String>,
    /// The connection changed while the job ran, which stopped it.
    pub connection_changed: bool,
}

/// What a job checks between objects.
pub trait Control {
    fn cancelled(&self) -> bool;
    fn progress(&self, done: u64, total: u64);
}

impl Control for JobContext {
    fn cancelled(&self) -> bool {
        JobContext::cancelled(self)
    }

    fn progress(&self, done: u64, total: u64) {
        JobContext::progress(self, done, total)
    }
}

/// Sets the ACL of each key in turn, while the connection is the one the
/// user confirmed (`identity`) and the user has not cancelled.
pub fn run(
    keys: &[String],
    identity: &str,
    backend: &dyn AccessBackend,
    control: &dyn Control,
) -> Report {
    let mut report = Report::default();
    let total = keys.len() as u64;
    for (index, key) in keys.iter().enumerate() {
        if control.cancelled() {
            report.left = keys.len() - index;
            return report;
        }
        if backend.identity().as_deref() != Some(identity) {
            report.connection_changed = true;
            report.left = keys.len() - index;
            return report;
        }
        match backend.set(key) {
            Ok(()) => report.changed += 1,
            Err(error) => {
                report.failed += 1;
                report.first_error.get_or_insert(error);
                // Refused at the first object, the rest would be refused
                // alike: the service or the bucket does not allow it.
                if report.changed == 0 {
                    report.left = keys.len() - index - 1;
                    return report;
                }
            }
        }
        control.progress(index as u64 + 1, total);
    }
    report
}

/// How the job ends in the task center: completed only when every object
/// was changed; cancelled when the user stopped it with nothing failed;
/// otherwise failed, saying what was done.
pub fn outcome(
    report: &Report,
    cancelled: bool,
    summary: impl Fn(&Report) -> String,
) -> Result<(), JobError> {
    if report.failed == 0 && report.left == 0 && !report.connection_changed {
        Ok(())
    } else if cancelled && report.failed == 0 && !report.connection_changed {
        Err(JobError::Cancelled)
    } else {
        Err(JobError::Failed(summary(report)))
    }
}

#[cfg(test)]
mod tests {
    use super::{outcome, run, AccessBackend, Control, Report};
    use crate::app_jobs::JobError;
    use std::cell::{Cell, RefCell};

    struct Fake {
        refuse: &'static str,
        identity: RefCell<Vec<&'static str>>,
        set: RefCell<Vec<String>>,
    }

    impl AccessBackend for Fake {
        fn set(&self, key: &str) -> Result<(), String> {
            self.set.borrow_mut().push(key.to_string());
            if key == self.refuse {
                Err(format!("refused {key}"))
            } else {
                Ok(())
            }
        }

        fn identity(&self) -> Option<String> {
            let mut identities = self.identity.borrow_mut();
            let now = if identities.len() > 1 {
                identities.remove(0)
            } else {
                identities[0]
            };
            Some(now.to_string())
        }
    }

    struct Stop(Cell<usize>);

    impl Control for Stop {
        fn cancelled(&self) -> bool {
            let left = self.0.get();
            self.0.set(left.saturating_sub(1));
            left == 0
        }

        fn progress(&self, _: u64, _: u64) {}
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn every_object_is_tried_and_failures_are_counted() {
        let fake = Fake {
            refuse: "b",
            identity: RefCell::new(vec!["id"]),
            set: RefCell::default(),
        };
        let report = run(&keys(&["a", "b", "c"]), "id", &fake, &Stop(Cell::new(9)));
        assert_eq!(fake.set.borrow().len(), 3);
        assert_eq!((report.changed, report.failed, report.left), (2, 1, 0));
        assert!(
            matches!(outcome(&report, false, |_| "summary".into()), Err(JobError::Failed(s)) if s == "summary")
        );

        // Refused at the very first object: the rest is not tried.
        let fake = Fake {
            refuse: "a",
            identity: RefCell::new(vec!["id"]),
            set: RefCell::default(),
        };
        let report = run(&keys(&["a", "b", "c"]), "id", &fake, &Stop(Cell::new(9)));
        assert_eq!(*fake.set.borrow(), ["a"]);
        assert_eq!((report.changed, report.failed, report.left), (0, 1, 2));
        assert_eq!(report.first_error.as_deref(), Some("refused a"));
    }

    #[test]
    fn a_cancel_or_an_edited_connection_stops_before_the_next_object() {
        let fake = Fake {
            refuse: "",
            identity: RefCell::new(vec!["id"]),
            set: RefCell::default(),
        };
        let report = run(&keys(&["a", "b", "c"]), "id", &fake, &Stop(Cell::new(1)));
        assert_eq!((report.changed, report.left), (1, 2));
        assert_eq!(
            outcome(&report, true, |_| String::new()),
            Err(JobError::Cancelled)
        );

        let fake = Fake {
            refuse: "",
            identity: RefCell::new(vec!["id", "other"]),
            set: RefCell::default(),
        };
        let report = run(&keys(&["a", "b"]), "id", &fake, &Stop(Cell::new(9)));
        assert_eq!(*fake.set.borrow(), ["a"]);
        assert!(report.connection_changed);
        assert_eq!(report.left, 1);
        assert!(outcome(&report, false, |_| String::new()).is_err());

        let all = Report {
            changed: 2,
            ..Report::default()
        };
        assert_eq!(outcome(&all, false, |_| String::new()), Ok(()));
    }
}
