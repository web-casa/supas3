// SPDX-License-Identifier: AGPL-3.0-or-later
//! Searches a location and everything below it for names containing some
//! text, and lists what was found. Lives inside a dialog the workspace
//! opens; opening a result or its folder is left to the workspace.

use crate::app_state::AppBackend;
use crate::error_details::ErrorDetails;
use crate::listing::{format_modified, format_size, Entry, EntryKind};
use crate::location::Location;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use supas3_core::commands::objects::SearchObjectsResult;

/// How many results are drawn; the count covers all of them.
const SHOWN: usize = 1000;

pub enum SearchViewEvent {
    /// Open this file, which is in `Location`.
    Open(Location, Entry),
    /// Show the folder a result is in.
    Reveal(Location),
}

enum State {
    Searching,
    Done(SearchObjectsResult),
    Failed(String),
}

pub struct SearchView {
    /// Where the search started: a prefix in a bucket.
    location: Location,
    query: String,
    state: State,
}

impl EventEmitter<SearchViewEvent> for SearchView {}

impl SearchView {
    pub fn new(location: Location, query: String, cx: &mut Context<Self>) -> Self {
        let view = Self {
            location: location.clone(),
            query: query.clone(),
            state: State::Searching,
        };
        if let Some(requests) = AppBackend::requests(cx) {
            cx.spawn(async move |view, cx| {
                let result = requests.search(location, query).await;
                let _ = view.update(cx, |view, cx| {
                    view.state = match result {
                        Ok(found) => State::Done(found),
                        Err(error) => State::Failed(error),
                    };
                    cx.notify();
                });
            })
            .detach();
        }
        view
    }

    /// The folder holding `key`, and the row that stands for it there.
    fn place_of(
        &self,
        key: &str,
        size: i64,
        modified: Option<String>,
    ) -> Option<(Location, Entry)> {
        let Location::Objects {
            profile_id, bucket, ..
        } = &self.location
        else {
            return None;
        };
        let (folder, name) = match key.rsplit_once('/') {
            Some((folder, name)) => (format!("{folder}/"), name),
            None => (String::new(), key),
        };
        Some((
            Location::objects(profile_id, bucket, &folder),
            Entry {
                name: name.to_string(),
                kind: EntryKind::File,
                size: u64::try_from(size).ok(),
                modified,
                prefix: None,
            },
        ))
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let prefix = match &self.location {
            Location::Objects { prefix, .. } => prefix.clone(),
            _ => String::new(),
        };
        let body = match &self.state {
            State::Searching => div()
                .py_4()
                .text_sm()
                .text_color(muted)
                .child(t!("search.running").into_owned())
                .into_any_element(),
            State::Failed(error) => ErrorDetails::new("search-error", error.clone())
                .danger()
                .into_any_element(),
            State::Done(found) => {
                let summary = if found.is_truncated {
                    t!(
                        "search.truncated",
                        count = found.objects.len(),
                        scanned = found.scanned_objects
                    )
                } else {
                    t!(
                        "search.complete",
                        count = found.objects.len(),
                        scanned = found.scanned_objects
                    )
                };
                let rows: Vec<AnyElement> = found
                    .objects
                    .iter()
                    .take(SHOWN)
                    .enumerate()
                    .filter_map(|(index, object)| {
                        let (folder, entry) =
                            self.place_of(&object.key, object.size, object.last_modified.clone())?;
                        let shown = object.key.strip_prefix(&prefix).unwrap_or(&object.key);
                        let (open_folder, open_entry) = (folder.clone(), entry.clone());
                        Some(
                            h_flex()
                                .w_full()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .text_sm()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(div().flex_1().min_w_0().truncate().child(shown.to_string()))
                                .child(
                                    div()
                                        .w(rems(5.))
                                        .flex_shrink_0()
                                        .text_right()
                                        .text_color(muted)
                                        .child(entry.size.map(format_size).unwrap_or_default()),
                                )
                                .child(
                                    div().w(rems(9.)).flex_shrink_0().text_color(muted).child(
                                        entry
                                            .modified
                                            .as_deref()
                                            .map(format_modified)
                                            .unwrap_or_default(),
                                    ),
                                )
                                .child(
                                    Button::new(("open-result", index))
                                        .ghost()
                                        .xsmall()
                                        .label(t!("action.open").into_owned())
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(SearchViewEvent::Open(
                                                open_folder.clone(),
                                                open_entry.clone(),
                                            ));
                                        })),
                                )
                                .child(
                                    Button::new(("reveal-result", index))
                                        .ghost()
                                        .xsmall()
                                        .label(t!("search.show_folder").into_owned())
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(SearchViewEvent::Reveal(folder.clone()));
                                        })),
                                )
                                .into_any_element(),
                        )
                    })
                    .collect();
                let hidden = found.objects.len().saturating_sub(SHOWN);
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(summary.into_owned()),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .max_h(rems(24.).to_pixels(window.rem_size()))
                            .overflow_y_scrollbar()
                            .children(rows)
                            .when(hidden > 0, |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(t!("search.more", count = hidden).into_owned()),
                                )
                            }),
                    )
                    .into_any_element()
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
                    .child(
                        t!(
                            "search.where",
                            query = self.query.as_str(),
                            prefix = prefix.as_str()
                        )
                        .into_owned(),
                    ),
            )
            .child(body)
            .child(
                h_flex().justify_end().child(
                    Button::new("close-search")
                        .outline()
                        .small()
                        .label(t!("action.close").into_owned())
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    }
}
