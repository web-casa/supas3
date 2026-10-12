// SPDX-License-Identifier: AGPL-3.0-or-later
//! The strip at the bottom of the window that shows transfers.

use crate::app_state::AppBackend;
use crate::listing::format_size;
use crate::tasks::{format_speed, EventQueue, Task, TaskFilter, TaskKind, TaskState, Tasks};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::sync::Arc;
use std::time::Duration;

/// How often waiting events are collected and shown.
const REFRESH: Duration = Duration::from_millis(200);

/// How many rows are drawn to begin with, and how many more each time the
/// user asks; every task stays reachable.
const ROWS_PER_STEP: usize = 200;

/// Emitted when transfers have finished, so panes can show the result.
pub enum TaskCenterEvent {
    TransfersFinished,
}

pub struct TaskCenter {
    tasks: Tasks,
    filter: TaskFilter,
    expanded: bool,
    /// How many rows of the current filter are drawn.
    row_limit: usize,
    /// The last request that failed, shown until the next action.
    error: Option<String>,
}

impl EventEmitter<TaskCenterEvent> for TaskCenter {}

impl TaskCenter {
    pub fn new(queue: Arc<EventQueue>, cx: &mut Context<Self>) -> Self {
        // Transfers journaled by an earlier run are listed from the start.
        if let Some(requests) = AppBackend::requests(cx) {
            cx.spawn(async move |center, cx| {
                if let Ok(jobs) = requests.transfers().await {
                    let _ = center.update(cx, |center, cx| {
                        center.tasks.merge_snapshot(&jobs);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        cx.spawn(async move |center, cx| loop {
            cx.background_executor().timer(REFRESH).await;
            let collected = queue.collect();
            if collected.is_empty() {
                continue;
            }
            let overflowed = collected.overflowed;
            let alive = center.update(cx, |center, cx| {
                let mut finished = false;
                for job in &collected.added {
                    finished |= center.tasks.add(job);
                }
                for update in &collected.updates {
                    let was_active = center
                        .tasks
                        .get(&update.job_id)
                        .is_some_and(|task| task.state.is_active());
                    if center.tasks.update(update) && was_active {
                        let ended = center
                            .tasks
                            .get(&update.job_id)
                            .is_some_and(|task| task.state == TaskState::Completed);
                        finished |= ended;
                    }
                }
                // New work is worth seeing; open the strip when it arrives.
                if !collected.added.is_empty() {
                    center.expanded = true;
                    center.filter = TaskFilter::Current;
                }
                if finished {
                    cx.emit(TaskCenterEvent::TransfersFinished);
                }
                cx.notify();
            });
            // Too many announcements arrived to keep; read the full list.
            if overflowed {
                let requests = cx.update(|cx| AppBackend::requests(cx));
                if let Some(requests) = requests {
                    if let Ok(jobs) = requests.transfers().await {
                        let _ = center.update(cx, |center, cx| {
                            center.tasks.merge_snapshot(&jobs);
                            cx.notify();
                        });
                    }
                }
            }
            if alive.is_err() {
                break;
            }
        })
        .detach();
        Self {
            tasks: Tasks::default(),
            filter: TaskFilter::default(),
            expanded: false,
            row_limit: ROWS_PER_STEP,
            error: None,
        }
    }

    /// Runs a request for one task and removes it from the list when asked.
    fn act(&mut self, id: String, action: TaskAction, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        self.error = None;
        cx.spawn(async move |center, cx| {
            let result = match action {
                TaskAction::Cancel => requests.cancel_transfer(id.clone()).await.map(|_| true),
                // `None`: the backend has nothing to retry it with.
                TaskAction::Retry => requests
                    .retry_transfer(id.clone())
                    .await
                    .map(|retried| retried.is_some()),
                TaskAction::Remove => requests.remove_transfer(id.clone()).await.map(|_| true),
            };
            let _ = center.update(cx, |center, cx| {
                match result {
                    Ok(_) if action == TaskAction::Remove => center.tasks.remove(&id),
                    Ok(true) => {}
                    Ok(false) => center.error = Some(t!("tasks.cannot_retry").into_owned()),
                    Err(error) => center.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Cancels everything that is waiting or running.
    fn cancel_all(&mut self, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        let ids = self.tasks.cancellable_ids();
        self.error = None;
        cx.spawn(async move |center, cx| {
            if let Err(error) = requests.cancel_transfers(ids).await {
                let _ = center.update(cx, |center, cx| {
                    center.error = Some(error);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Removes completed and cancelled transfers. Failed ones stay until the
    /// user removes them: their error is the only record of what went wrong.
    fn clear_finished(&mut self, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        let ids = self.tasks.settled_ids();
        self.error = None;
        cx.spawn(async move |center, cx| {
            let result = requests.remove_transfers(ids.clone()).await;
            let _ = center.update(cx, |center, cx| {
                match result {
                    Ok(_) => {
                        for id in &ids {
                            center.tasks.remove(id);
                        }
                    }
                    Err(error) => center.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn summary(&self) -> String {
        let counts = self.tasks.counts();
        if counts.active == 0 && counts.failed == 0 {
            return t!("tasks.idle").into_owned();
        }
        let mut parts = Vec::new();
        if counts.active > 0 {
            let (done, total, speed) = self.tasks.active_progress();
            parts.push(t!("tasks.active", count = counts.active).into_owned());
            if total > 0 {
                parts.push(format!("{} / {}", format_size(done), format_size(total)));
            }
            if speed > 0.0 {
                parts.push(format_speed(speed));
            }
        }
        if counts.failed > 0 {
            parts.push(t!("tasks.failed", count = counts.failed).into_owned());
        }
        parts.join(" · ")
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let counts = self.tasks.counts();
        let filters = [
            TaskFilter::Current,
            TaskFilter::Failed,
            TaskFilter::Finished,
        ];
        let selected = filters
            .iter()
            .position(|filter| *filter == self.filter)
            .unwrap_or(0);
        h_flex()
            .w_full()
            .h_8()
            .px_2()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("toggle-tasks")
                    .ghost()
                    .small()
                    .icon(if self.expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronUp
                    })
                    .label(t!("tasks.title").into_owned())
                    .on_click(cx.listener(|center, _, _, cx| {
                        center.expanded = !center.expanded;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if counts.failed > 0 {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(self.summary()),
            )
            .when(self.expanded, |this| {
                this.child(
                    TabBar::new("task-filter")
                        .segmented()
                        .small()
                        .selected_index(selected)
                        .on_click(cx.listener(move |center, index: &usize, _, cx| {
                            center.filter = filters[(*index).min(filters.len() - 1)];
                            center.row_limit = ROWS_PER_STEP;
                            cx.notify();
                        }))
                        .child(Tab::new().label(t!("tasks.filter_current").into_owned()))
                        .child(
                            Tab::new().label(
                                t!("tasks.filter_failed", count = counts.failed).into_owned(),
                            ),
                        )
                        .child(Tab::new().label(t!("tasks.filter_finished").into_owned())),
                )
                .child(
                    Button::new("cancel-all")
                        .ghost()
                        .small()
                        .label(t!("tasks.cancel_all").into_owned())
                        .disabled(counts.active == 0)
                        .on_click(cx.listener(|center, _, _, cx| center.cancel_all(cx))),
                )
                .child(
                    Button::new("clear-finished")
                        .ghost()
                        .small()
                        .label(t!("tasks.clear_finished").into_owned())
                        .disabled(counts.finished == 0)
                        .on_click(cx.listener(|center, _, _, cx| center.clear_finished(cx))),
                )
            })
    }

    fn render_task(&self, task: &Task, cx: &mut Context<Self>) -> impl IntoElement {
        let id = task.id.clone();
        let button_id = |name: &str| SharedString::from(format!("task-{name}-{}", task.id));
        let (kind_icon, kind_label) = match task.kind {
            TaskKind::Upload => (IconName::ArrowUp, t!("tasks.upload")),
            TaskKind::Download => (IconName::ArrowDown, t!("tasks.download")),
            TaskKind::Copy => (IconName::Copy, t!("tasks.copy")),
        };
        let (state_text, danger) = match &task.state {
            TaskState::Queued => (t!("tasks.state_queued").into_owned(), false),
            TaskState::Running => (
                match task.fraction() {
                    Some(fraction) => format!(
                        "{:.0}% · {}",
                        fraction * 100.0,
                        format_speed(task.bytes_per_second)
                    ),
                    None => format_speed(task.bytes_per_second),
                },
                false,
            ),
            TaskState::Paused => (t!("tasks.state_paused").into_owned(), false),
            TaskState::Completed => (t!("tasks.state_completed").into_owned(), false),
            TaskState::Cancelled => (t!("tasks.state_cancelled").into_owned(), false),
            TaskState::Failed(message) => {
                (t!("tasks.state_failed", error = message).into_owned(), true)
            }
        };
        let action = |name: &str, label: String, action: TaskAction, cx: &mut Context<Self>| {
            let id = id.clone();
            Button::new(button_id(name))
                .ghost()
                .xsmall()
                .label(label)
                .on_click(cx.listener(move |center, _, _, cx| {
                    center.act(id.clone(), action, cx);
                }))
        };

        h_flex()
            .id(SharedString::from(format!("task-{}", task.id)))
            .w_full()
            .px_3()
            .py_1()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Icon::new(kind_icon)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().truncate().child(task.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(kind_label.into_owned()),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} → {}", task.source, task.target)),
                    )
                    .when(danger, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(state_text.clone()),
                        )
                    }),
            )
            .when(task.state == TaskState::Running, |this| {
                this.child(
                    div().w_32().child(
                        Progress::new(SharedString::from(format!("progress-{}", task.id)))
                            .value(task.fraction().unwrap_or(0.0) * 100.0),
                    ),
                )
            })
            .when(!danger, |this| {
                this.child(
                    div()
                        .w_40()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(state_text.clone()),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .when(task.state.can_cancel(), |this| {
                        this.child(action(
                            "cancel",
                            t!("action.cancel").into_owned(),
                            TaskAction::Cancel,
                            cx,
                        ))
                    })
                    .when(task.state.has_stopped_short() && task.retryable, |this| {
                        this.child(action(
                            "retry",
                            t!("action.retry").into_owned(),
                            TaskAction::Retry,
                            cx,
                        ))
                    })
                    .when(!task.state.is_active(), |this| {
                        this.child(action(
                            "remove",
                            t!("tasks.remove").into_owned(),
                            TaskAction::Remove,
                            cx,
                        ))
                    }),
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskAction {
    Cancel,
    Retry,
    Remove,
}

impl Render for TaskCenter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Nothing below the header is built while the strip is collapsed.
        let (rows, hidden): (Vec<AnyElement>, usize) = if self.expanded {
            let listed = self.tasks.list(self.filter);
            let hidden = listed.len().saturating_sub(self.row_limit);
            let rows = listed
                .into_iter()
                .take(self.row_limit)
                .map(|task| self.render_task(task, cx).into_any_element())
                .collect();
            (rows, hidden)
        } else {
            (Vec::new(), 0)
        };
        let empty = rows.is_empty();

        v_flex()
            .w_full()
            .bg(cx.theme().background)
            .child(self.render_header(cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when(self.expanded, |this| {
                this.child(
                    div()
                        .id("task-list")
                        .h_48()
                        .overflow_y_scroll()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .when(empty, |this| {
                            this.child(
                                div()
                                    .p_4()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(t!("tasks.none").into_owned()),
                            )
                        })
                        .children(rows)
                        .when(hidden > 0, |this| {
                            this.child(
                                h_flex().px_3().py_2().child(
                                    Button::new("show-more-tasks")
                                        .ghost()
                                        .small()
                                        .label(
                                            t!(
                                                "tasks.show_more",
                                                count = hidden.min(ROWS_PER_STEP),
                                                hidden = hidden
                                            )
                                            .into_owned(),
                                        )
                                        .on_click(cx.listener(|center, _, _, cx| {
                                            center.row_limit += ROWS_PER_STEP;
                                            cx.notify();
                                        })),
                                ),
                            )
                        }),
                )
            })
    }
}
