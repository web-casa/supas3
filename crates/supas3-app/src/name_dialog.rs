// SPDX-License-Identifier: AGPL-3.0-or-later
//! Asks for one name: a new folder's, or a new name for a file or folder.
//! The workspace carries out the change and reports back.

use crate::error_details::ErrorDetails;
use crate::file_ops::NameProblem;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

pub enum NameDialogEvent {
    /// The user confirmed a valid name.
    Submitted(String),
}

pub struct NameDialog {
    input: Entity<InputState>,
    /// The name as it was, to tell an unchanged rename from a new name.
    original: Option<String>,
    check: fn(&str) -> Result<(), NameProblem>,
    /// Names already present; a new name must not be one of them.
    taken: Vec<String>,
    confirm_label: SharedString,
    problem: Option<String>,
    busy: bool,
    error: Option<String>,
    /// Focus and selection to apply once shown: the dialog takes focus as
    /// it opens, so doing it earlier is undone.
    pending_selection: Option<std::ops::Range<usize>>,
    _changes: Subscription,
}

impl EventEmitter<NameDialogEvent> for NameDialog {}

impl NameDialog {
    /// `selected` is the part of `initial` to select, as a byte range.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial: &str,
        selected: std::ops::Range<usize>,
        original: Option<String>,
        check: fn(&str) -> Result<(), NameProblem>,
        taken: Vec<String>,
        confirm_label: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial.to_string()));
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
        Self {
            input,
            original,
            check,
            taken,
            confirm_label,
            problem: None,
            busy: false,
            error: None,
            pending_selection: Some(selected),
            _changes: changes,
        }
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let name = self.input.read(cx).value().to_string();
        let problem = match (self.check)(&name) {
            Err(problem) => Some(t!(problem.message_key()).into_owned()),
            Ok(()) if self.original.as_deref() == Some(name.as_str()) => {
                Some(t!("name.unchanged").into_owned())
            }
            Ok(()) if self.taken.contains(&name) => {
                Some(t!("name.exists", name = name.as_str()).into_owned())
            }
            Ok(()) => None,
        };
        if problem.is_some() {
            self.problem = problem;
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.emit(NameDialogEvent::Submitted(name));
        cx.notify();
    }

    /// The change failed; the dialog stays open so the user can try again.
    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(error);
        cx.notify();
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

impl Render for NameDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(range) = self.pending_selection.take() {
            let input = self.input.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |input, cx| {
                    input.focus(window, cx);
                    input.set_selected_range(range, cx);
                });
            });
        }
        v_flex()
            .w_full()
            .gap_3()
            .child(Input::new(&self.input).disabled(self.busy))
            .when_some(self.problem.clone(), |this, problem| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(problem))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(ErrorDetails::new("name-error", error).danger())
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-name")
                            .outline()
                            .label(t!("action.cancel").into_owned())
                            .disabled(self.busy)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("confirm-name")
                            .primary()
                            .label(self.confirm_label.clone())
                            .loading(self.busy)
                            .disabled(self.busy)
                            .on_click(cx.listener(|dialog, _, _, cx| dialog.submit(cx))),
                    ),
            )
    }
}
