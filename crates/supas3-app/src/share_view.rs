// SPDX-License-Identifier: AGPL-3.0-or-later
//! Makes links to one object: a signed link that expires, and the object's
//! plain address. Lives inside a dialog the workspace opens.

use crate::app_state::AppBackend;
use crate::object_actions::{brief_error, Expiry, ObjectRef};
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
    StyledExt as _, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

enum Link {
    Loading,
    Ready(SharedString),
    Failed(String),
}

pub struct ShareView {
    object: ObjectRef,
    expiry: Expiry,
    signed: Link,
    /// `None` when the address cannot be worked out for this connection.
    plain: Option<Link>,
    /// Links signed with temporary credentials can stop working early.
    may_expire_early: bool,
    /// Counts signing requests, so only the latest answer is shown.
    generation: u64,
}

impl ShareView {
    pub fn new(
        object: ObjectRef,
        may_expire_early: bool,
        plain_address: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            object,
            expiry: Expiry::Hour,
            signed: Link::Loading,
            plain: plain_address.then_some(Link::Loading),
            may_expire_early,
            generation: 0,
        };
        view.sign(cx);
        if plain_address {
            view.load_plain(cx);
        }
        view
    }

    fn sign(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.signed = Link::Loading;
        cx.notify();
        let Some(requests) = AppBackend::requests(cx) else {
            self.signed = Link::Failed(t!("error.backend_stopped").into_owned());
            return;
        };
        let (object, seconds) = (self.object.clone(), self.expiry.seconds());
        cx.spawn(async move |view, cx| {
            let result = requests.signed_link(object, seconds).await;
            let _ = view.update(cx, |view, cx| {
                if view.generation == generation {
                    view.signed = match result {
                        Ok(link) => Link::Ready(link.into()),
                        Err(error) => Link::Failed(error),
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn load_plain(&mut self, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            self.plain = Some(Link::Failed(t!("error.backend_stopped").into_owned()));
            return;
        };
        let object = self.object.clone();
        cx.spawn(async move |view, cx| {
            let result = requests.plain_link(object).await;
            let _ = view.update(cx, |view, cx| {
                view.plain = Some(match result {
                    Ok(link) => Link::Ready(link.into()),
                    Err(error) => Link::Failed(error),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn render_link(
        &self,
        id: &'static str,
        link: &Link,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (text, color, ready) = match link {
            Link::Loading => (
                SharedString::from(t!("share.preparing").into_owned()),
                cx.theme().muted_foreground,
                None,
            ),
            Link::Ready(link) => (link.clone(), cx.theme().foreground, Some(link.clone())),
            Link::Failed(error) => (
                SharedString::from(t!("share.failed", detail = brief_error(error)).into_owned()),
                cx.theme().danger,
                None,
            ),
        };
        h_flex()
            .w_full()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .text_color(color)
                    .truncate()
                    .child(text),
            )
            .child(
                Button::new(id)
                    .outline()
                    .small()
                    .label(t!("share.copy").into_owned())
                    .disabled(ready.is_none())
                    .on_click(move |_, window, cx| {
                        if let Some(link) = &ready {
                            cx.write_to_clipboard(ClipboardItem::new_string(link.to_string()));
                            window.push_notification(
                                (
                                    NotificationType::Success,
                                    SharedString::from(t!("share.copied").into_owned()),
                                ),
                                cx,
                            );
                        }
                    }),
            )
    }
}

impl Render for ShareView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let expiry = self.expiry;
        v_flex()
            .w_full()
            .gap_4()
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(self.object.display()),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(t!("share.signed_title").into_owned()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("share.signed_note").into_owned()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .child(t!("share.expires_after").into_owned()),
                            )
                            .children(Expiry::ALL.into_iter().enumerate().map(
                                |(index, choice)| {
                                    Button::new(("expiry", index))
                                        .outline()
                                        .small()
                                        .label(t!(choice.label_key()).into_owned())
                                        .selected(choice == expiry)
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            if view.expiry != choice {
                                                view.expiry = choice;
                                                view.sign(cx);
                                            }
                                        }))
                                },
                            )),
                    )
                    .child(self.render_link("copy-signed", &self.signed, cx))
                    .when(self.may_expire_early, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child(t!("share.early_expiry").into_owned()),
                        )
                    }),
            )
            .when_some(self.plain.as_ref(), |this, plain| {
                this.child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .child(t!("share.plain_title").into_owned()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("share.plain_note").into_owned()),
                        )
                        .child(self.render_link("copy-plain", plain, cx)),
                )
            })
            .child(
                h_flex().justify_end().child(
                    Button::new("close-share")
                        .outline()
                        .label(t!("action.close").into_owned())
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    }
}
