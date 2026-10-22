// SPDX-License-Identifier: AGPL-3.0-or-later
//! Asks for web addresses to import into a folder of a bucket. The
//! workspace queues them and reports back.

use crate::error_details::ErrorDetails;
use crate::object_actions::{import_lines, ImportLine, IMPORT_LIMIT};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// The size limit offered at first, in MiB.
const DEFAULT_LIMIT_MIB: u64 = 5 * 1024;
/// The largest limit accepted, in MiB: the backend's 5 TiB.
const MAX_LIMIT_MIB: u64 = 5 * 1024 * 1024;

pub enum ImportEvent {
    /// Addresses with the names of their objects, and the size limit in
    /// bytes.
    Submitted(Vec<(String, String)>, u64),
}

pub struct ImportDialog {
    addresses: Entity<TextareaState>,
    limit: Entity<InputState>,
    lines: Vec<ImportLine>,
    problem: Option<String>,
    busy: bool,
    error: Option<String>,
    /// Focus to apply once shown: the dialog takes focus as it opens.
    focus_pending: bool,
    _changes: Vec<Subscription>,
}

impl EventEmitter<ImportEvent> for ImportDialog {}

impl ImportDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let addresses = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(t!("import.placeholder").into_owned())
        });
        let limit =
            cx.new(|cx| InputState::new(window, cx).default_value(DEFAULT_LIMIT_MIB.to_string()));
        let changes = vec![
            cx.subscribe(
                &addresses,
                |dialog: &mut Self, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        dialog.lines = import_lines(&input.read(cx).value());
                        dialog.problem = None;
                        dialog.error = None;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(&limit, |dialog: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    dialog.problem = None;
                    cx.notify();
                }
            }),
        ];
        Self {
            addresses,
            limit,
            lines: Vec::new(),
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
        // What is in the box now, however it got there.
        self.lines = import_lines(&self.addresses.read(cx).value());
        let limit = self
            .limit
            .read(cx)
            .value()
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|mib| (1..=MAX_LIMIT_MIB).contains(mib));
        let named: Option<Vec<(String, String)>> = self
            .lines
            .iter()
            .map(|line| line.name.clone().ok().map(|name| (line.url.clone(), name)))
            .collect();
        self.problem = match (limit, named) {
            (None, _) => Some(t!("import.limit_invalid").into_owned()),
            (_, None) => Some(t!("import.fix_lines").into_owned()),
            (_, Some(named)) if named.is_empty() => Some(t!("import.none").into_owned()),
            (_, Some(named)) if named.len() > IMPORT_LIMIT => {
                Some(t!("import.too_many", limit = IMPORT_LIMIT).into_owned())
            }
            (Some(limit), Some(named)) => {
                self.busy = true;
                self.error = None;
                cx.emit(ImportEvent::Submitted(named, limit * 1024 * 1024));
                None
            }
        };
        cx.notify();
    }

    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(error);
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

impl Render for ImportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            let addresses = self.addresses.clone();
            window.defer(cx, move |window, cx| {
                addresses.update(cx, |input, cx| input.focus(window, cx));
            });
        }
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let height = rems(9.).to_pixels(window.rem_size());
        let ready = self.lines.iter().filter(|line| line.name.is_ok()).count();
        let troubled: Vec<&ImportLine> = self
            .lines
            .iter()
            .filter(|line| line.name.is_err())
            .take(5)
            .collect();
        v_flex()
            .w_full()
            .gap_3()
            .child(Textarea::new(&self.addresses).h(height).disabled(self.busy))
            .when(!self.lines.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(t!("import.ready", count = ready).into_owned()),
                )
            })
            .children(troubled.into_iter().map(|line| {
                let why = line
                    .name
                    .clone()
                    .err()
                    .map(|problem| t!(problem.message_key()));
                div().text_xs().text_color(danger).truncate().child(format!(
                    "{} — {}",
                    line.url,
                    why.unwrap_or_default()
                ))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child(t!("import.limit").into_owned()))
                    .child(
                        div()
                            .w(rems(8.))
                            .child(Input::new(&self.limit).disabled(self.busy)),
                    )
                    .child(div().text_sm().text_color(muted).child("MiB")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("import.note").into_owned()),
            )
            .when_some(self.problem.clone(), |this, problem| {
                this.child(div().text_sm().text_color(danger).child(problem))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(ErrorDetails::new("import-error", error).danger())
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-import")
                            .outline()
                            .label(t!("action.cancel").into_owned())
                            .disabled(self.busy)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("start-import")
                            .primary()
                            .label(t!("import.start").into_owned())
                            .loading(self.busy)
                            .disabled(self.busy)
                            .on_click(cx.listener(|dialog, _, _, cx| dialog.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ImportDialog, ImportEvent};
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
    use gpui_kit::{
        div, px, size, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
        Point, Render, Styled as _, Subscription, TestAppContext, Window, WindowBounds,
        WindowOptions,
    };
    use std::time::Duration;

    struct Host {
        dialog: Entity<ImportDialog>,
        submitted: Vec<(Vec<(String, String)>, u64)>,
        _events: Subscription,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().p_4().child(self.dialog.clone())
        }
    }

    #[gpui_kit::test]
    async fn typed_addresses_are_named_and_sent(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (window, host) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(700.), px(600.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    let dialog = cx.new(|cx| ImportDialog::new(window, cx));
                    let events = cx.subscribe(&dialog, |host: &mut Host, _, event, _| {
                        let ImportEvent::Submitted(items, limit) = event;
                        host.submitted.push((items.clone(), *limit));
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
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let addresses = host.read(cx).dialog.read(cx).addresses.clone();
            addresses.update(cx, |input, cx| input.focus(window, cx));
            window.input("https://example.com/a/report%20final.pdf", cx);
            window.press("enter", cx);
            window.input("ftp://x/y", cx);
            window.click("start-import", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // A line that cannot be imported holds the rest back.
        cx.update(|cx| {
            let host = host.read(cx);
            assert!(host.submitted.is_empty());
            assert_eq!(host.dialog.read(cx).lines.len(), 2);
        });
        cx.update_window(window, |_, window, cx| {
            // The bad line taken out.
            let addresses = host.read(cx).dialog.read(cx).addresses.clone();
            addresses.update(cx, |input, cx| {
                input.set_value("https://example.com/a/report%20final.pdf", window, cx)
            });
            window.render_frame(cx);
            window.click("start-import", cx);
        })
        .unwrap();
        cx.wait_for(window, Duration::from_secs(1), |_, cx| {
            !host.read(cx).submitted.is_empty()
        })
        .await;
        cx.update(|cx| {
            let (items, limit) = host.read(cx).submitted[0].clone();
            assert_eq!(
                items,
                [(
                    "https://example.com/a/report%20final.pdf".to_string(),
                    "report final.pdf".to_string()
                )]
            );
            assert_eq!(limit, 5 * 1024 * 1024 * 1024);
        });
    }
}
