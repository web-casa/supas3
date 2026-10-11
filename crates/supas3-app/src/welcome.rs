// SPDX-License-Identifier: AGPL-3.0-or-later
//! The placeholder window content shown until the workspace exists.

use crate::backend::BackendStatus;
use gpui_kit::component::*;
use gpui_kit::*;
use rust_i18n::t;

pub struct Welcome {
    status: BackendStatus,
}

impl Welcome {
    pub fn new(status: BackendStatus) -> Self {
        Self { status }
    }
}

/// The localized sentence describing the backend state.
pub fn status_text(status: &BackendStatus) -> String {
    match status {
        BackendStatus::Ready { connections: 0 } => t!("status.ready_none").into_owned(),
        BackendStatus::Ready { connections } => {
            t!("status.ready_count", count = connections).into_owned()
        }
        BackendStatus::Failed { error } => t!("status.failed", error = error).into_owned(),
    }
}

impl Render for Welcome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let failed = matches!(self.status, BackendStatus::Failed { .. });
        let status_color = if failed {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };

        div()
            .v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .p_6()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_2xl().font_semibold().child("supaS3"))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("app.tagline").into_owned()),
            )
            .child(
                div()
                    .mt_4()
                    .text_sm()
                    .text_color(status_color)
                    .child(status_text(&self.status)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("app.version", version = env!("CARGO_PKG_VERSION")).into_owned()),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: that would pull in GPUI's `test` attribute macro and
    // shadow the standard `#[test]`.
    use super::status_text;
    use crate::backend::BackendStatus;

    // rust-i18n keeps one process-wide locale, so every locale-dependent
    // assertion lives in this single test.
    #[test]
    fn status_text_is_translated_for_each_state_and_language() {
        let ready = BackendStatus::Ready { connections: 3 };
        let failed = BackendStatus::Failed {
            error: "disk full".into(),
        };

        rust_i18n::set_locale(crate::i18n::ENGLISH);
        assert_eq!(
            status_text(&BackendStatus::Ready { connections: 0 }),
            "Backend ready. No connections yet."
        );
        assert_eq!(status_text(&ready), "Backend ready. Connections: 3");
        assert_eq!(
            status_text(&failed),
            "The backend could not start: disk full"
        );

        rust_i18n::set_locale(crate::i18n::SIMPLIFIED_CHINESE);
        assert_eq!(
            status_text(&BackendStatus::Ready { connections: 0 }),
            "后端已就绪，尚未添加连接。"
        );
        assert_eq!(status_text(&ready), "后端已就绪，连接数：3");
        assert_eq!(status_text(&failed), "后端启动失败：disk full");
    }
}
