// SPDX-License-Identifier: AGPL-3.0-or-later
//! Asks how many transfers run at once and how fast each may go. The
//! workspace applies the limits and saves them.

use crate::error_details::ErrorDetails;
use crate::settings::{TransferProblem, TransferSettings, CONCURRENCY};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

pub enum TransfersDialogEvent {
    /// The user confirmed valid limits.
    Submitted(TransferSettings),
}

pub struct TransfersDialog {
    concurrency: Entity<InputState>,
    bandwidth: Entity<InputState>,
    problem: Option<TransferProblem>,
    busy: bool,
    error: Option<String>,
    /// Focus to apply once shown: the dialog takes focus as it opens.
    focus_pending: bool,
    _changes: Vec<Subscription>,
}

impl EventEmitter<TransfersDialogEvent> for TransfersDialog {}

impl TransfersDialog {
    pub fn new(current: TransferSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let concurrency =
            cx.new(|cx| InputState::new(window, cx).default_value(current.concurrency.to_string()));
        let bandwidth = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("transfers.unlimited").into_owned())
                .default_value(current.bandwidth_text())
        });
        let changes = [&concurrency, &bandwidth]
            .into_iter()
            .map(|input| {
                cx.subscribe(
                    input,
                    |dialog: &mut Self, _, event: &InputEvent, cx| match event {
                        InputEvent::Change => {
                            dialog.problem = None;
                            dialog.error = None;
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => dialog.submit(cx),
                        _ => {}
                    },
                )
            })
            .collect();
        Self {
            concurrency,
            bandwidth,
            problem: None,
            busy: false,
            error: None,
            focus_pending: true,
            _changes: changes,
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let concurrency = self.concurrency.read(cx).value().to_string();
        let bandwidth = self.bandwidth.read(cx).value().to_string();
        match TransferSettings::parse(&concurrency, &bandwidth) {
            Ok(limits) => {
                self.busy = true;
                self.error = None;
                cx.emit(TransfersDialogEvent::Submitted(limits));
            }
            Err(problem) => self.problem = Some(problem),
        }
        cx.notify();
    }

    /// Applying the limits failed; the dialog stays open.
    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(error);
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

impl Render for TransfersDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            let input = self.concurrency.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |input, cx| input.focus(window, cx));
            });
        }
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let problem = self.problem;
        let field = |id: &'static str,
                     label: String,
                     input: &Entity<InputState>,
                     suffix: Option<String>,
                     concerns: TransferProblem| {
            v_flex()
                .gap_1()
                .child(div().text_sm().child(label))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Input::new(input).id(id).disabled(self.busy))
                        .when_some(suffix, |this, suffix| {
                            this.child(div().text_sm().text_color(muted).child(suffix))
                        }),
                )
                // What is wrong shows beside what is wrong.
                .when(problem == Some(concerns), |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(danger)
                            .child(t!(concerns.message_key()).into_owned()),
                    )
                })
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(field(
                "transfers-concurrency",
                t!(
                    "transfers.concurrency",
                    min = CONCURRENCY.start(),
                    max = CONCURRENCY.end()
                )
                .into_owned(),
                &self.concurrency,
                None,
                TransferProblem::Concurrency,
            ))
            .child(field(
                "transfers-bandwidth",
                t!("transfers.bandwidth").into_owned(),
                &self.bandwidth,
                Some("MiB/s".to_string()),
                TransferProblem::Bandwidth,
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("transfers.note").into_owned()),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(ErrorDetails::new("transfers-error", error).danger())
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-transfers")
                            .outline()
                            .label(t!("action.cancel").into_owned())
                            .disabled(self.busy)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("save-transfers")
                            .primary()
                            .label(t!("transfers.save").into_owned())
                            .loading(self.busy)
                            .disabled(self.busy)
                            .on_click(cx.listener(|dialog, _, _, cx| dialog.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{TransfersDialog, TransfersDialogEvent};
    use crate::settings::TransferSettings;
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        div, px, size, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
        Point, Render, Styled as _, Subscription, TestAppContext, Window, WindowBounds,
        WindowOptions,
    };
    use std::time::Duration;

    struct Host {
        dialog: Entity<TransfersDialog>,
        submitted: Vec<TransferSettings>,
        _events: Subscription,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().p_4().child(self.dialog.clone())
        }
    }

    #[gpui_kit::test]
    async fn transfer_limits_are_checked_before_they_are_applied(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (window, host) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(600.), px(500.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    let dialog =
                        cx.new(|cx| TransfersDialog::new(TransferSettings::default(), window, cx));
                    let events = cx.subscribe(&dialog, |host: &mut Host, _, event, _| {
                        let TransfersDialogEvent::Submitted(limits) = event;
                        host.submitted.push(*limits);
                    });
                    Host {
                        dialog,
                        submitted: Vec::new(),
                        _events: events,
                    }
                })
            })
            .expect("open test window")
        });

        // Out of range: the dialog says so and asks for nothing.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("transfers-concurrency", cx);
            window.press("backspace", cx);
            window.input("30", cx);
            window.click("save-transfers", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let host = host.read(cx);
            assert!(host.submitted.is_empty());
            assert!(!host.dialog.read(cx).is_busy());
        });

        // In range, with a speed limit in MiB/s.
        cx.update_window(window, |_, window, cx| {
            window.click("transfers-concurrency", cx);
            window.press("backspace", cx);
            window.press("backspace", cx);
            window.input("8", cx);
            window.click("transfers-bandwidth", cx);
            window.input("2.5", cx);
            window.click("save-transfers", cx);
        })
        .unwrap();
        cx.wait_for(window, Duration::from_secs(1), |_, cx| {
            !host.read(cx).submitted.is_empty()
        })
        .await;
        cx.update(|cx| {
            let host = host.read(cx);
            assert_eq!(
                host.submitted,
                vec![TransferSettings {
                    concurrency: 8,
                    bandwidth_kib: 2560,
                }]
            );
            assert!(
                host.dialog.read(cx).is_busy(),
                "a second save waits for this one"
            );
        });
    }
}
