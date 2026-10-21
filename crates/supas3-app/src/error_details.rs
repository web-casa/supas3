// SPDX-License-Identifier: AGPL-3.0-or-later
//! A storage error as people read it: what it most likely means first, with
//! the service's own wording one click away for when that is not enough.

use crate::connection::error_hint_key;
use crate::object_actions::brief_error;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// The summary line of an error: the likely cause, then the start of the
/// service's message.
pub fn summary(error: &str) -> String {
    let brief = brief_error(error);
    match error_hint_key(error) {
        Some(key) => format!("{} ({brief})", t!(key)),
        None => brief,
    }
}

#[derive(IntoElement)]
pub struct ErrorDetails {
    id: ElementId,
    error: SharedString,
    danger: bool,
}

impl ErrorDetails {
    pub fn new(id: impl Into<ElementId>, error: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            error: error.into(),
            danger: false,
        }
    }

    /// Shows the summary in the danger color.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

impl RenderOnce for ErrorDetails {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let open = window.use_keyed_state(self.id.clone(), cx, |_, _| false);
        let shown = *open.read(cx);
        let summary = summary(&self.error);
        // Only offer details that say more than the summary already does.
        let has_more = self.error.trim() != brief_error(&self.error);
        let raw = self.error.clone();
        v_flex()
            .id(self.id.clone())
            .w_full()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .when(self.danger, |this| this.text_color(cx.theme().danger))
                    .child(summary),
            )
            .when(has_more, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(("toggle", 0usize))
                                .ghost()
                                .xsmall()
                                .label(if shown {
                                    t!("error.hide_details").into_owned()
                                } else {
                                    t!("error.show_details").into_owned()
                                })
                                .on_click(move |_, _, cx| {
                                    open.update(cx, |open, cx| {
                                        *open = !*open;
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Clipboard::new(("copy", 0usize))
                                .value(raw.clone())
                                .tooltip(t!("error.copy_details").into_owned()),
                        ),
                )
            })
            .when(has_more && shown, |this| {
                this.child(
                    div()
                        .id(("details", 0usize))
                        .w_full()
                        .max_h(rems(12.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().muted)
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(self.error.clone()),
                )
            })
    }
}
