// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dialogs for buckets: making one, deleting one, and editing its policy.
//! The workspace carries out the change and reports back.

use crate::buckets::bucket_name_problem;
use crate::connection::Service;
use crate::error_details::ErrorDetails;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// What a bucket dialog asks the workspace to do.
pub enum BucketEvent {
    /// Make a bucket of this name in this region.
    Create(String, String),
    /// Delete the bucket; the name was typed to confirm.
    Delete(String),
    /// Save this policy, or remove it with `None`.
    SavePolicy(Option<String>),
}

/// The state every bucket dialog shares: whether it waits for an answer,
/// and the last error.
#[derive(Default)]
struct Progress {
    busy: bool,
    problem: Option<String>,
    error: Option<String>,
}

fn buttons(
    id: &'static str,
    confirm: String,
    danger: bool,
    progress: &Progress,
    ready: bool,
    submit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let confirm = Button::new(id)
        .label(confirm)
        .loading(progress.busy)
        .disabled(progress.busy || !ready)
        .on_click(submit);
    h_flex()
        .gap_2()
        .justify_end()
        .child(
            Button::new("cancel-bucket")
                .outline()
                .label(t!("action.cancel").into_owned())
                .disabled(progress.busy)
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(if danger {
            confirm.danger()
        } else {
            confirm.primary()
        })
}

fn messages(progress: &Progress, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_2()
        .when_some(progress.problem.clone(), |this, problem| {
            this.child(div().text_sm().text_color(cx.theme().danger).child(problem))
        })
        .when_some(progress.error.clone(), |this, error| {
            this.child(ErrorDetails::new("bucket-error", error).danger())
        })
}

/// Asks for a new bucket's name and, on AWS, its region.
pub struct CreateBucketDialog {
    service: Service,
    name: Entity<InputState>,
    region: Entity<InputState>,
    progress: Progress,
    focus_pending: bool,
    _changes: Vec<Subscription>,
}

impl EventEmitter<BucketEvent> for CreateBucketDialog {}

impl CreateBucketDialog {
    pub fn new(
        service: Service,
        region: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx));
        let region = {
            let region = region.to_string();
            cx.new(|cx| InputState::new(window, cx).default_value(region))
        };
        let changes = [&name, &region]
            .into_iter()
            .map(|input| {
                cx.subscribe(
                    input,
                    |dialog: &mut Self, _, event: &InputEvent, cx| match event {
                        InputEvent::Change => {
                            dialog.progress.problem = None;
                            dialog.progress.error = None;
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => dialog.submit(cx),
                        _ => {}
                    },
                )
            })
            .collect();
        Self {
            service,
            name,
            region,
            progress: Progress::default(),
            focus_pending: true,
            _changes: changes,
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.progress.busy {
            return;
        }
        let name = self.name.read(cx).value().trim().to_string();
        let region = self.region.read(cx).value().trim().to_string();
        if let Some(problem) = bucket_name_problem(&name, self.service, &region) {
            self.progress.problem = Some(t!(problem.message_key()).into_owned());
        } else if region.is_empty() {
            self.progress.problem = Some(t!("bucket.region_missing").into_owned());
        } else {
            self.progress.busy = true;
            self.progress.error = None;
            cx.emit(BucketEvent::Create(name, region));
        }
        cx.notify();
    }

    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.progress.busy = false;
        self.progress.error = Some(error);
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.progress.busy
    }
}

impl Render for CreateBucketDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            let input = self.name.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |input, cx| input.focus(window, cx));
            });
        }
        let muted = cx.theme().muted_foreground;
        // Elsewhere a bucket is made where the connection's endpoint is.
        let choose_region = self.service == Service::Aws;
        let rule = match self.service {
            Service::Cos => t!("bucket.rule_cos"),
            Service::Aws | Service::Generic => t!("bucket.rule_dots"),
            _ => t!("bucket.rule"),
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(div().text_sm().child(t!("bucket.name").into_owned()))
            .child(Input::new(&self.name).disabled(self.progress.busy))
            .child(div().text_xs().text_color(muted).child(rule.into_owned()))
            .child(div().text_sm().child(t!("bucket.region").into_owned()))
            .child(Input::new(&self.region).disabled(self.progress.busy || !choose_region))
            .when(!choose_region, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("bucket.region_fixed").into_owned()),
                )
            })
            .child(messages(&self.progress, cx))
            .child(buttons(
                "create-bucket",
                t!("bucket.create").into_owned(),
                false,
                &self.progress,
                true,
                cx.listener(|dialog, _, _, cx| dialog.submit(cx)),
            ))
    }
}

/// Asks for a bucket's name to be typed before deleting it.
pub struct DeleteBucketDialog {
    bucket: String,
    typed: Entity<InputState>,
    progress: Progress,
    focus_pending: bool,
    _changes: Subscription,
}

impl EventEmitter<BucketEvent> for DeleteBucketDialog {}

impl DeleteBucketDialog {
    pub fn new(bucket: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let typed = cx.new(|cx| InputState::new(window, cx));
        let changes = cx.subscribe(&typed, |dialog: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                dialog.progress.error = None;
                cx.notify();
            }
        });
        Self {
            bucket,
            typed,
            progress: Progress::default(),
            focus_pending: true,
            _changes: changes,
        }
    }

    fn confirmed(&self, cx: &App) -> bool {
        self.typed.read(cx).value() == self.bucket.as_str()
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.progress.busy || !self.confirmed(cx) {
            return;
        }
        self.progress.busy = true;
        self.progress.error = None;
        cx.emit(BucketEvent::Delete(self.typed.read(cx).value().to_string()));
        cx.notify();
    }

    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.progress.busy = false;
        self.progress.error = Some(error);
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.progress.busy
    }
}

impl Render for DeleteBucketDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            let input = self.typed.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |input, cx| input.focus(window, cx));
            });
        }
        let ready = self.confirmed(cx);
        v_flex()
            .w_full()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .child(t!("bucket.delete_body").into_owned()),
            )
            .child(
                div()
                    .text_sm()
                    .child(t!("bucket.delete_type", name = self.bucket.as_str()).into_owned()),
            )
            .child(Input::new(&self.typed).disabled(self.progress.busy))
            .child(messages(&self.progress, cx))
            .child(buttons(
                "delete-bucket",
                t!("bucket.delete").into_owned(),
                true,
                &self.progress,
                ready,
                cx.listener(|dialog, _, _, cx| dialog.submit(cx)),
            ))
    }
}

/// Shows a bucket's policy as JSON and saves an edited one.
pub struct PolicyDialog {
    original: Option<String>,
    text: Entity<TextareaState>,
    progress: Progress,
    _changes: Subscription,
}

impl EventEmitter<BucketEvent> for PolicyDialog {}

/// A policy as shown: indented JSON, or the text as it came when it is not
/// JSON.
pub fn pretty_policy(policy: &str) -> String {
    serde_json::from_str::<serde_json::Value>(policy)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| policy.to_string())
}

impl PolicyDialog {
    pub fn new(original: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shown = original.as_deref().map(pretty_policy).unwrap_or_default();
        let text = cx.new(|cx| TextareaState::new(window, cx).default_value(shown));
        let changes = cx.subscribe(&text, |dialog: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                dialog.progress.problem = None;
                dialog.progress.error = None;
                cx.notify();
            }
        });
        Self {
            original,
            text,
            progress: Progress::default(),
            _changes: changes,
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.progress.busy {
            return;
        }
        let typed = self.text.read(cx).value().trim().to_string();
        let policy = if typed.is_empty() {
            None
        } else {
            match serde_json::from_str::<serde_json::Value>(&typed) {
                Ok(value) if value.is_object() => Some(value.to_string()),
                Ok(_) => {
                    self.progress.problem = Some(t!("policy.not_object").into_owned());
                    cx.notify();
                    return;
                }
                Err(error) => {
                    self.progress.problem =
                        Some(t!("policy.invalid", detail = error.to_string()).into_owned());
                    cx.notify();
                    return;
                }
            }
        };
        let unchanged = match (&policy, &self.original) {
            (None, None) => true,
            (Some(new), Some(old)) => {
                serde_json::from_str::<serde_json::Value>(new).ok()
                    == serde_json::from_str::<serde_json::Value>(old).ok()
            }
            _ => false,
        };
        if unchanged {
            self.progress.problem = Some(t!("policy.unchanged").into_owned());
        } else {
            self.progress.busy = true;
            self.progress.error = None;
            cx.emit(BucketEvent::SavePolicy(policy));
        }
        cx.notify();
    }

    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.progress.busy = false;
        self.progress.error = Some(error);
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.progress.busy
    }

    pub fn original(&self) -> Option<String> {
        self.original.clone()
    }
}

impl Render for PolicyDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let height = rems(18.).to_pixels(window.rem_size());
        v_flex()
            .w_full()
            .gap_3()
            .child(
                Textarea::new(&self.text)
                    .h(height)
                    .disabled(self.progress.busy),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .whitespace_normal()
                    .child(t!("policy.note").into_owned()),
            )
            .child(messages(&self.progress, cx))
            .child(buttons(
                "save-policy",
                t!("action.save").into_owned(),
                false,
                &self.progress,
                true,
                cx.listener(|dialog, _, _, cx| dialog.submit(cx)),
            ))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn policies_are_shown_indented() {
        let compact = r#"{"Version":"2012-10-17","Statement":[]}"#;
        let shown = super::pretty_policy(compact);
        assert!(shown.contains('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&shown).unwrap(),
            serde_json::from_str::<serde_json::Value>(compact).unwrap()
        );
        assert_eq!(super::pretty_policy("not json"), "not json");
    }
}
