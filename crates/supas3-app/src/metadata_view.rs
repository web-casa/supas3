// SPDX-License-Identifier: AGPL-3.0-or-later
//! Edits an object's user metadata. The workspace makes the change and
//! reports back.

use crate::error_details::ErrorDetails;
use crate::object_actions::{fixed_metadata_key, metadata_to_save, MetadataProblem};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::collections::HashMap;

/// The most rows offered; far more than fit in 2 KiB of metadata.
const MAX_ROWS: usize = 64;

pub enum MetadataEvent {
    /// The user confirmed valid metadata, different from what was there.
    Submitted(HashMap<String, String>),
}

struct Row {
    key: Entity<InputState>,
    value: Entity<InputState>,
}

pub struct MetadataDialog {
    original: HashMap<String, String>,
    idrive_e2: bool,
    rows: Vec<Row>,
    problem: Option<MetadataProblem>,
    busy: bool,
    error: Option<String>,
    _changes: Vec<Subscription>,
}

impl EventEmitter<MetadataEvent> for MetadataDialog {}

impl MetadataDialog {
    pub fn new(
        original: HashMap<String, String>,
        idrive_e2: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editable: Vec<(String, String)> = original
            .iter()
            .filter(|(key, _)| !fixed_metadata_key(key, idrive_e2))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        editable.sort();
        let mut dialog = Self {
            original,
            idrive_e2,
            rows: Vec::new(),
            problem: None,
            busy: false,
            error: None,
            _changes: Vec::new(),
        };
        for (key, value) in editable {
            dialog.add_row(&key, &value, window, cx);
        }
        if dialog.rows.is_empty() {
            dialog.add_row("", "", window, cx);
        }
        dialog
    }

    fn add_row(&mut self, key: &str, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut input = |text: &str, placeholder: String, cx: &mut Context<Self>| {
            let text = text.to_string();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(text)
            })
        };
        let key = input(key, t!("metadata.key").into_owned(), cx);
        let value = input(value, t!("metadata.value").into_owned(), cx);
        for field in [&key, &value] {
            self._changes.push(cx.subscribe(
                field,
                |dialog: &mut Self, _, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        dialog.problem = None;
                        dialog.error = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => dialog.submit(cx),
                    _ => {}
                },
            ));
        }
        self.rows.push(Row { key, value });
    }

    fn typed(&self, cx: &App) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|row| {
                (
                    row.key.read(cx).value().to_string(),
                    row.value.read(cx).value().to_string(),
                )
            })
            .collect()
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match metadata_to_save(&self.typed(cx), &self.original, self.idrive_e2) {
            Ok(metadata) if metadata == self.original => {
                self.error = Some(t!("metadata.unchanged").into_owned());
            }
            Ok(metadata) => {
                self.busy = true;
                self.error = None;
                cx.emit(MetadataEvent::Submitted(metadata));
            }
            Err(problem) => self.problem = Some(problem),
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

impl Render for MetadataDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let problem_row = self.problem.and_then(MetadataProblem::row);
        let mut fixed: Vec<_> = self
            .original
            .iter()
            .filter(|(key, _)| fixed_metadata_key(key, self.idrive_e2))
            .collect();
        fixed.sort();
        v_flex()
            .w_full()
            .gap_2()
            .when(!self.rows.is_empty(), |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(muted)
                        .child(div().w(rems(10.)).child(t!("metadata.key").into_owned()))
                        .child(div().flex_1().child(t!("metadata.value").into_owned())),
                )
            })
            .children(self.rows.iter().enumerate().map(|(index, row)| {
                h_flex()
                    .gap_2()
                    .when(problem_row == Some(index), |this| {
                        this.border_1().border_color(danger).rounded_md()
                    })
                    .child(
                        div()
                            .w(rems(10.))
                            .child(Input::new(&row.key).disabled(self.busy)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&row.value).disabled(self.busy)),
                    )
                    .child(
                        Button::new(("remove-metadata", index))
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(t!("metadata.remove").into_owned())
                            .disabled(self.busy)
                            .on_click(cx.listener(move |dialog, _, _, cx| {
                                if index < dialog.rows.len() {
                                    dialog.rows.remove(index);
                                    dialog.problem = None;
                                    cx.notify();
                                }
                            })),
                    )
            }))
            .child(
                h_flex().child(
                    Button::new("add-metadata")
                        .ghost()
                        .small()
                        .icon(IconName::Plus)
                        .label(t!("metadata.add").into_owned())
                        .disabled(self.busy || self.rows.len() >= MAX_ROWS)
                        .on_click(cx.listener(|dialog, _, window, cx| {
                            dialog.add_row("", "", window, cx);
                            cx.notify();
                        })),
                ),
            )
            .children(fixed.into_iter().map(|(key, value)| {
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("metadata.fixed", key = key, value = value).into_owned())
            }))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("metadata.note").into_owned()),
            )
            .when_some(self.problem, |this, problem| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(danger)
                        .child(t!(problem.message_key()).into_owned()),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(ErrorDetails::new("metadata-error", error).danger())
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-metadata")
                            .outline()
                            .label(t!("action.cancel").into_owned())
                            .disabled(self.busy)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("save-metadata")
                            .primary()
                            .label(t!("action.save").into_owned())
                            .loading(self.busy)
                            .disabled(self.busy)
                            .on_click(cx.listener(|dialog, _, _, cx| dialog.submit(cx))),
                    ),
            )
    }
}
