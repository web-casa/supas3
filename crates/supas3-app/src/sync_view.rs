// SPDX-License-Identifier: AGPL-3.0-or-later
//! Syncs a local folder to a prefix: compare first, show what would be
//! uploaded, then start. Lives inside a dialog the workspace opens.

use crate::app_state::AppBackend;
use crate::listing::format_size;
use crate::object_actions::{brief_error, sync_action_key, SyncCounts, SyncPair};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use supas3_core::commands::sync::Preview;

/// How many compared files are listed; the counts cover all of them.
const LISTED: usize = 200;

enum Comparison {
    NotRun,
    Running,
    Done(Preview),
    Failed(String),
}

pub struct SyncView {
    pair: SyncPair,
    /// Leave files that already exist remotely alone, without comparing.
    skip_existing: bool,
    comparison: Comparison,
    /// The user agreed to replace remote objects that differ.
    replace_agreed: bool,
    starting: bool,
    start_error: Option<String>,
    /// Counts comparisons, so only the latest answer is shown.
    generation: u64,
}

impl SyncView {
    pub fn new(pair: SyncPair, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            pair,
            skip_existing: false,
            comparison: Comparison::NotRun,
            replace_agreed: false,
            starting: false,
            start_error: None,
            generation: 0,
        };
        view.compare(cx);
        view
    }

    fn counts(&self) -> SyncCounts {
        match &self.comparison {
            Comparison::Done(preview) => SyncCounts {
                new_files: preview.new_files,
                changed_files: preview.changed_files,
                unverified_files: preview.unverified_files,
            },
            _ => SyncCounts::default(),
        }
    }

    fn compare(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.replace_agreed = false;
        self.start_error = None;
        let Some(requests) = AppBackend::requests(cx) else {
            self.comparison = Comparison::Failed(t!("error.backend_stopped").into_owned());
            cx.notify();
            return;
        };
        self.comparison = Comparison::Running;
        cx.notify();
        let (pair, skip_existing) = (self.pair.clone(), self.skip_existing);
        cx.spawn(async move |view, cx| {
            let result = requests.compare_for_sync(pair, skip_existing).await;
            let _ = view.update(cx, |view, cx| {
                if view.generation == generation {
                    view.comparison = match result {
                        Ok(preview) => Comparison::Done(preview),
                        Err(error) => Comparison::Failed(error),
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Comparison::Done(preview) = &self.comparison else {
            return;
        };
        if self.starting || !self.counts().may_start(self.replace_agreed) {
            return;
        }
        let Some(requests) = AppBackend::requests(cx) else {
            self.start_error = Some(t!("error.backend_stopped").into_owned());
            cx.notify();
            return;
        };
        let (profile_id, plan_id) = (self.pair.profile_id.clone(), preview.id.clone());
        let replace = self.counts().replaced() > 0 && self.replace_agreed;
        self.starting = true;
        self.start_error = None;
        cx.notify();
        cx.spawn_in(window, async move |view, cx| {
            let result = requests.start_sync(profile_id, plan_id, replace).await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.starting = false;
                match result {
                    Ok(count) => {
                        window.close_dialog(cx);
                        window.push_notification(
                            (
                                NotificationType::Success,
                                SharedString::from(t!("sync.started", count = count).into_owned()),
                            ),
                            cx,
                        );
                    }
                    Err(error) => {
                        // A plan can be used once and expires; compare again.
                        view.start_error = Some(error);
                        view.comparison = Comparison::NotRun;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_summary(&self, preview: &Preview, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let line = |label: String, count: usize| {
            h_flex()
                .gap_2()
                .text_sm()
                .child(div().w(rems(4.)).text_right().child(count.to_string()))
                .child(div().text_color(muted).child(label))
        };
        v_flex()
            .gap_1()
            .child(line(t!("sync.count_new").into_owned(), preview.new_files))
            .child(line(
                t!("sync.count_changed").into_owned(),
                preview.changed_files,
            ))
            .when(preview.unverified_files > 0, |this| {
                this.child(line(
                    t!("sync.count_unverified").into_owned(),
                    preview.unverified_files,
                ))
            })
            .child(line(
                t!("sync.count_unchanged").into_owned(),
                preview.unchanged_files,
            ))
            .when(preview.skipped_files > 0, |this| {
                this.child(line(
                    t!("sync.count_skipped").into_owned(),
                    preview.skipped_files,
                ))
            })
            .when(preview.remote_only_files > 0, |this| {
                this.child(line(
                    t!("sync.count_remote_only").into_owned(),
                    preview.remote_only_files,
                ))
            })
            .child(
                div().pt_1().text_sm().child(
                    t!(
                        "sync.upload_total",
                        size = format_size(preview.upload_bytes)
                    )
                    .into_owned(),
                ),
            )
    }

    fn render_entries(
        &self,
        preview: &Preview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        // What will be uploaded comes first; unchanged files are left out.
        let listed: Vec<_> = preview
            .entries
            .iter()
            .filter(|entry| entry.action != "Unchanged")
            .collect();
        let hidden = listed.len().saturating_sub(LISTED);
        let prefix_len = self.pair.prefix.len();
        v_flex()
            .w_full()
            .max_h(rems(12.).to_pixels(window.rem_size()))
            .overflow_y_scrollbar()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .children(listed.into_iter().take(LISTED).map(|entry| {
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .text_xs()
                    .child(
                        div().flex_1().min_w_0().truncate().child(
                            entry
                                .key
                                .get(prefix_len..)
                                .unwrap_or(&entry.key)
                                .to_string(),
                        ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(muted)
                            .child(format_size(entry.size)),
                    )
                    .child(
                        div()
                            .w(rems(6.))
                            .flex_shrink_0()
                            .text_color(muted)
                            .child(t!(sync_action_key(&entry.action)).into_owned()),
                    )
            }))
            .when(hidden > 0, |this| {
                this.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("sync.more_not_listed", count = hidden).into_owned()),
                )
            })
    }
}

impl Render for SyncView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let counts = self.counts();
        let busy = self.starting || matches!(self.comparison, Comparison::Running);
        let result = match &self.comparison {
            Comparison::NotRun => div()
                .text_sm()
                .text_color(muted)
                .child(t!("sync.not_compared").into_owned())
                .into_any_element(),
            Comparison::Running => div()
                .text_sm()
                .text_color(muted)
                .child(t!("sync.comparing").into_owned())
                .into_any_element(),
            Comparison::Failed(error) => div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(t!("sync.compare_failed", detail = brief_error(error)).into_owned())
                .into_any_element(),
            Comparison::Done(preview) => v_flex()
                .gap_3()
                .child(self.render_summary(preview, cx))
                .when(counts.uploads() > 0, |this| {
                    this.child(self.render_entries(preview, window, cx))
                })
                .when(counts.uploads() == 0, |this| {
                    this.child(div().text_sm().text_color(muted).child(
                        if preview.skipped_files > 0 {
                            // Skipped files were not compared; they may differ.
                            t!("sync.nothing_with_options").into_owned()
                        } else {
                            t!("sync.nothing_to_upload").into_owned()
                        },
                    ))
                })
                .into_any_element(),
        };
        let place = |label: String, value: String| {
            h_flex()
                .gap_2()
                .text_sm()
                .child(
                    div()
                        .w(rems(4.))
                        .flex_shrink_0()
                        .text_color(muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().truncate().child(value))
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(place(
                t!("sync.from").into_owned(),
                self.pair.local.display().to_string(),
            ))
            .child(place(
                t!("sync.to").into_owned(),
                self.pair.target_display(),
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("sync.one_way_note").into_owned()),
            )
            .child(
                Checkbox::new("skip-existing")
                    .label(t!("sync.skip_existing").into_owned())
                    .checked(self.skip_existing)
                    .disabled(busy)
                    .on_change(cx.listener(|view, checked: &bool, _, cx| {
                        // The comparison no longer matches the option.
                        view.skip_existing = *checked;
                        view.compare(cx);
                    })),
            )
            .child(result)
            .when(counts.replaced() > 0, |this| {
                this.child(
                    Checkbox::new("agree-replace")
                        .label(t!("sync.agree_replace", count = counts.replaced()).into_owned())
                        .checked(self.replace_agreed)
                        .disabled(busy)
                        .on_change(cx.listener(|view, checked: &bool, _, cx| {
                            view.replace_agreed = *checked;
                            cx.notify();
                        })),
                )
            })
            .when_some(self.start_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(t!("sync.start_failed", detail = brief_error(&error)).into_owned()),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("compare-again")
                            .ghost()
                            .label(t!("sync.compare_again").into_owned())
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, _, cx| view.compare(cx))),
                    )
                    .child(
                        Button::new("cancel-sync")
                            .outline()
                            .label(t!("action.cancel").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("start-sync")
                            .primary()
                            .label(t!("sync.start").into_owned())
                            .loading(self.starting)
                            .disabled(busy || !counts.may_start(self.replace_agreed))
                            .on_click(cx.listener(|view, _, window, cx| view.start(window, cx))),
                    ),
            )
    }
}
