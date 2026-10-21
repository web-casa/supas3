// SPDX-License-Identifier: AGPL-3.0-or-later
//! What the service reports about one object. Lives inside a dialog the
//! workspace opens.

use crate::app_state::AppBackend;
use crate::error_details::ErrorDetails;
use crate::listing::{format_modified, format_size};
use crate::object_actions::ObjectRef;
use gpui_kit::component::button::Button;
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, WindowExt as _};
use gpui_kit::*;
use rust_i18n::t;
use supas3_core::commands::operations::ObjectMetadata;

enum State {
    Loading,
    Loaded(ObjectMetadata),
    Failed(String),
}

pub struct PropertiesView {
    object: ObjectRef,
    state: State,
}

impl PropertiesView {
    pub fn new(object: ObjectRef, cx: &mut Context<Self>) -> Self {
        let view = Self {
            object: object.clone(),
            state: State::Loading,
        };
        if let Some(requests) = AppBackend::requests(cx) {
            cx.spawn(async move |view, cx| {
                let result = requests.object_properties(object).await;
                let _ = view.update(cx, |view, cx| {
                    view.state = match result {
                        Ok(metadata) => State::Loaded(metadata),
                        Err(error) => State::Failed(error),
                    };
                    cx.notify();
                });
            })
            .detach();
        }
        view
    }
}

impl Render for PropertiesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let body = match &self.state {
            State::Loading => div()
                .py_4()
                .text_sm()
                .text_color(muted)
                .child(t!("view.loading").into_owned())
                .into_any_element(),
            State::Failed(error) => ErrorDetails::new("properties-error", error.clone())
                .danger()
                .into_any_element(),
            State::Loaded(metadata) => {
                let unknown = || t!("properties.unknown").into_owned();
                let mut list = DescriptionList::new()
                    .columns(1)
                    .label_width(rems(8.).to_pixels(window.rem_size()))
                    .item(t!("properties.key").into_owned(), metadata.key.clone(), 1)
                    .item(
                        t!("properties.size").into_owned(),
                        u64::try_from(metadata.size)
                            .map(|size| {
                                let short = format_size(size);
                                // The exact count only adds something above a kilobyte.
                                if short == format!("{size} B") {
                                    short
                                } else {
                                    format!("{short} ({size} B)")
                                }
                            })
                            .unwrap_or_else(|_| unknown()),
                        1,
                    )
                    .item(
                        t!("properties.modified").into_owned(),
                        metadata
                            .last_modified
                            .as_deref()
                            .map(format_modified)
                            .unwrap_or_else(unknown),
                        1,
                    )
                    .item(
                        t!("properties.content_type").into_owned(),
                        metadata.content_type.clone().unwrap_or_else(unknown),
                        1,
                    )
                    .item(
                        t!("properties.storage_class").into_owned(),
                        metadata.storage_class.clone().unwrap_or_else(unknown),
                        1,
                    )
                    .item("ETag", metadata.e_tag.clone().unwrap_or_else(unknown), 1);
                let mut user: Vec<_> = metadata.user_metadata.iter().collect();
                user.sort();
                for (name, value) in user {
                    list = list.item(format!("x-amz-meta-{name}"), value.clone(), 1);
                }
                list.into_any_element()
            }
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .truncate()
                    .child(self.object.display()),
            )
            .child(body)
            .child(
                h_flex().justify_end().child(
                    Button::new("close-properties")
                        .outline()
                        .label(t!("action.close").into_owned())
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    }
}
