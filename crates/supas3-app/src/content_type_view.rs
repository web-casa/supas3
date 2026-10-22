// SPDX-License-Identifier: AGPL-3.0-or-later
//! Asks for an object's new Content-Type. The workspace makes the change
//! and reports back.

use crate::error_details::ErrorDetails;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use supas3_core::s3::{infer_content_type, validate_content_type};

pub enum ContentTypeEvent {
    /// The user confirmed a valid type, different from the current one.
    Submitted(String),
}

pub struct ContentTypeDialog {
    input: Entity<InputState>,
    current: Option<String>,
    /// The type the name suggests, when it differs from the current one.
    suggested: Option<String>,
    problem: Option<String>,
    busy: bool,
    error: Option<String>,
    /// Focus to apply once shown: the dialog takes focus as it opens.
    focus_pending: bool,
    _changes: Subscription,
}

impl EventEmitter<ContentTypeEvent> for ContentTypeDialog {}

impl ContentTypeDialog {
    pub fn new(
        name: &str,
        current: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).default_value(current.clone().unwrap_or_default())
        });
        let changes =
            cx.subscribe(
                &input,
                |dialog: &mut Self, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        dialog.problem = None;
                        dialog.error = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => dialog.submit(cx),
                    _ => {}
                },
            );
        let suggested =
            Some(infer_content_type(name)).filter(|guess| Some(guess) != current.as_ref());
        Self {
            input,
            current,
            suggested,
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
        let typed = self.input.read(cx).value().to_string();
        match validate_content_type(&typed) {
            Ok(value) if Some(&value) == self.current.as_ref() => {
                self.problem = Some(t!("content_type.unchanged").into_owned());
            }
            Ok(value) => {
                self.busy = true;
                self.error = None;
                cx.emit(ContentTypeEvent::Submitted(value));
            }
            Err(error) => self.problem = Some(error.to_string()),
        }
        cx.notify();
    }

    /// The change failed; the dialog stays open.
    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(error);
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

impl Render for ContentTypeDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            let input = self.input.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |input, cx| input.focus(window, cx));
            });
        }
        let muted = cx.theme().muted_foreground;
        v_flex()
            .w_full()
            .gap_3()
            .child(
                Input::new(&self.input)
                    .id("content-type")
                    .disabled(self.busy),
            )
            .when_some(self.suggested.clone(), |this, suggested| {
                let input = self.input.clone();
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(t!("content_type.suggested").into_owned()),
                        )
                        .child(
                            Button::new("use-suggested")
                                .small()
                                .ghost()
                                .label(suggested.clone())
                                .disabled(self.busy)
                                .on_click(move |_, window, cx| {
                                    input.update(cx, |input, cx| {
                                        input.set_value(suggested.clone(), window, cx)
                                    });
                                }),
                        ),
                )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("content_type.note").into_owned()),
            )
            .when_some(self.problem.clone(), |this, problem| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(problem))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(ErrorDetails::new("content-type-error", error).danger())
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-content-type")
                            .outline()
                            .label(t!("action.cancel").into_owned())
                            .disabled(self.busy)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("save-content-type")
                            .primary()
                            .label(t!("content_type.save").into_owned())
                            .loading(self.busy)
                            .disabled(self.busy)
                            .on_click(cx.listener(|dialog, _, _, cx| dialog.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentTypeDialog, ContentTypeEvent};
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        div, px, size, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
        Point, Render, Styled as _, Subscription, TestAppContext, Window, WindowBounds,
        WindowOptions,
    };
    use std::time::Duration;

    struct Host {
        dialog: Entity<ContentTypeDialog>,
        submitted: Vec<String>,
        _events: Subscription,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().p_4().child(self.dialog.clone())
        }
    }

    #[gpui_kit::test]
    async fn the_type_by_name_is_offered_and_the_current_one_is_not_sent(cx: &mut TestAppContext) {
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
                    let dialog = cx.new(|cx| {
                        ContentTypeDialog::new(
                            "notes.md",
                            Some("application/octet-stream".into()),
                            window,
                            cx,
                        )
                    });
                    let events = cx.subscribe(&dialog, |host: &mut Host, _, event, _| {
                        let ContentTypeEvent::Submitted(value) = event;
                        host.submitted.push(value.clone());
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

        // Saving the type the object already has asks for nothing.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("save-content-type", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert!(host.read(cx).submitted.is_empty()));

        // The suggestion by name fills the field, and is what is sent.
        cx.update_window(window, |_, window, cx| {
            window.click("use-suggested", cx);
            window.render_frame(cx);
            assert_eq!(window.find("content-type").value(), Some("text/markdown"));
            window.click("save-content-type", cx);
        })
        .unwrap();
        cx.wait_for(window, Duration::from_secs(1), |_, cx| {
            !host.read(cx).submitted.is_empty()
        })
        .await;
        cx.update(|cx| assert_eq!(host.read(cx).submitted, ["text/markdown"]));
    }
}
