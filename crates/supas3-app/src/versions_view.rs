// SPDX-License-Identifier: AGPL-3.0-or-later
//! The version history of one object, with restoring an older version.
//! Lives inside a dialog the workspace opens.

use crate::app_state::AppBackend;
use crate::error_details::ErrorDetails;
use crate::listing::{format_modified, format_size};
use crate::object_actions::{
    can_delete_version, can_download_version, can_restore, short_version_id, version_delete_note,
    versioning_key, ObjectRef,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use supas3_core::s3::versions::{Cursor, Version};

/// What a version is waiting to be confirmed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Restore,
    Delete,
}

pub struct VersionsView {
    object: ObjectRef,
    /// The bucket's versioning status as the service reports it.
    versioning: Option<String>,
    versions: Vec<Version>,
    next: Option<Cursor>,
    loading: bool,
    error: Option<String>,
    /// What the user asked to do with a version and has not yet confirmed.
    confirming: Option<(Confirm, String)>,
    /// A restore or delete is running.
    restoring: bool,
    /// Counts reloads, so a late page of an older load is dropped.
    generation: u64,
}

impl VersionsView {
    pub fn new(object: ObjectRef, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            object,
            versioning: None,
            versions: Vec::new(),
            next: None,
            loading: false,
            error: None,
            confirming: None,
            restoring: false,
            generation: 0,
        };
        view.reload(cx);
        view
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.versions.clear();
        self.next = None;
        self.confirming = None;
        self.load(None, cx);
    }

    fn load(&mut self, cursor: Option<Cursor>, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            self.error = Some(t!("error.backend_stopped").into_owned());
            cx.notify();
            return;
        };
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        cx.notify();
        let object = self.object.clone();
        cx.spawn(async move |view, cx| {
            let result = requests.versions(object, cursor).await;
            let _ = view.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                view.loading = false;
                match result {
                    Ok(history) => {
                        view.versioning = Some(history.versioning);
                        view.versions.extend(history.versions);
                        view.next = history.next;
                    }
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The id of the version that is current now. Restoring names it, so a
    /// restore is refused if the object changed after the list was loaded.
    fn current_version(&self) -> Option<&str> {
        self.versions
            .iter()
            .find(|version| version.is_latest)
            .map(|version| version.version_id.as_str())
    }

    fn restore(&mut self, version_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(current), Some(requests)) = (
            self.current_version().map(str::to_string),
            AppBackend::requests(cx),
        ) else {
            return;
        };
        self.restoring = true;
        self.error = None;
        cx.notify();
        let object = self.object.clone();
        cx.spawn_in(window, async move |view, cx| {
            let result = requests.restore_version(object, version_id, current).await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.restoring = false;
                view.confirming = None;
                match result {
                    Ok(_) => window.push_notification(
                        (
                            NotificationType::Success,
                            SharedString::from(t!("versions.restore_queued").into_owned()),
                        ),
                        cx,
                    ),
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete(&mut self, version: Version, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .versions
            .iter()
            .find(|version| version.is_latest)
            .map(|current| supas3_core::commands::version_ops::SeenCurrent {
                version_id: current.version_id.clone(),
                etag: current.etag.clone(),
                is_delete_marker: current.is_delete_marker,
            });
        let (Some(current), Some(requests)) = (current, AppBackend::requests(cx)) else {
            return;
        };
        self.restoring = true;
        self.error = None;
        cx.notify();
        let object = self.object.clone();
        cx.spawn_in(window, async move |view, cx| {
            let result = requests
                .delete_version(
                    object,
                    version.version_id.clone(),
                    current,
                    version.is_delete_marker,
                )
                .await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.restoring = false;
                view.confirming = None;
                match result {
                    Ok(()) => {
                        window.push_notification(
                            (
                                NotificationType::Success,
                                SharedString::from(t!("versions.deleted").into_owned()),
                            ),
                            cx,
                        );
                        view.reload(cx);
                    }
                    Err(error) => {
                        // The history may have changed; show it as it is.
                        view.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Downloads a version into a folder the user picks, under a name not
    /// taken there.
    fn download(&mut self, version: Version, window: &mut Window, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("details.download_here").into_owned().into()),
        });
        let object = self.object.clone();
        cx.spawn_in(window, async move |view, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            let name = object
                .key
                .rsplit('/')
                .next()
                .unwrap_or(&object.key)
                .to_string();
            // The name must be a plain name on this computer, or the file
            // could land outside the folder picked.
            if !crate::transfer_plan::is_safe_local_name(&name) {
                let _ = view.update(cx, |view, cx| {
                    view.error =
                        Some(t!("versions.unsafe_name", name = name.as_str()).into_owned());
                    cx.notify();
                });
                return;
            }
            // The version goes into the name, so two versions downloaded
            // into one folder do not race for the same name.
            let range = crate::file_ops::stem_range(&name, false);
            let tag: String = version
                .version_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(8)
                .collect();
            let name = format!("{} ({tag}){}", &name[range.clone()], &name[range.end..]);
            let name = crate::file_ops::unique_name(&name, false, |candidate| {
                std::fs::symlink_metadata(folder.join(candidate)).is_ok()
            });
            let result = requests
                .download_version(
                    object,
                    version.version_id.clone(),
                    folder.join(&name),
                    version.size.unwrap_or(0),
                )
                .await;
            let _ = view.update_in(cx, |view, window, cx| {
                match result {
                    Ok(_) => window.push_notification(
                        (
                            NotificationType::Success,
                            SharedString::from(
                                t!("versions.download_queued", name = name.as_str()).into_owned(),
                            ),
                        ),
                        cx,
                    ),
                    Err(error) => view.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_version(
        &self,
        index: usize,
        version: &Version,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let id = version.version_id.clone();
        let confirming = self
            .confirming
            .as_ref()
            .filter(|(_, confirming)| *confirming == id)
            .map(|(what, _)| *what);
        let deletable = can_delete_version(version.is_latest, version.is_delete_marker)
            && self.current_version().is_some();
        let downloadable = can_download_version(version.is_delete_marker);
        let status = self.versioning.as_deref().unwrap_or_default();
        let restorable = can_restore(version.is_latest, version.is_delete_marker, status)
            && self.current_version().is_some();
        let modified = version
            .modified
            .as_deref()
            .map(format_modified)
            .unwrap_or_default();
        v_flex()
            .w_full()
            .px_2()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .text_sm()
                    .child(div().w(rems(11.)).flex_shrink_0().child(modified))
                    .child(
                        div()
                            .w(rems(5.))
                            .flex_shrink_0()
                            .text_right()
                            .text_color(muted)
                            .child(version.size.map(format_size).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .child(short_version_id(&id)),
                    )
                    .when(version.is_latest, |this| {
                        this.child(
                            Tag::primary()
                                .small()
                                .child(t!("versions.current").into_owned()),
                        )
                    })
                    .when(version.is_delete_marker, |this| {
                        this.child(
                            Tag::secondary()
                                .small()
                                .child(t!("versions.delete_marker").into_owned()),
                        )
                    })
                    .when(downloadable && confirming.is_none(), |this| {
                        let version = version.clone();
                        this.child(
                            Button::new(("download-version", index))
                                .ghost()
                                .xsmall()
                                .label(t!("details.download").into_owned())
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.download(version.clone(), window, cx);
                                })),
                        )
                    })
                    .when(restorable && confirming.is_none(), |this| {
                        let id = id.clone();
                        this.child(
                            Button::new(("restore", index))
                                .outline()
                                .xsmall()
                                .label(t!("versions.restore_ellipsis").into_owned())
                                .disabled(self.restoring)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.confirming = Some((Confirm::Restore, id.clone()));
                                    cx.notify();
                                })),
                        )
                    })
                    .when(deletable && confirming.is_none(), |this| {
                        let id = id.clone();
                        this.child(
                            Button::new(("delete-version", index))
                                .ghost()
                                .xsmall()
                                .label(t!("action.delete_ellipsis").into_owned())
                                .disabled(self.restoring)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.confirming = Some((Confirm::Delete, id.clone()));
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when(confirming == Some(Confirm::Delete), |this| {
                let note = version_delete_note(&self.versions, index);
                let version = version.clone();
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            v_flex()
                                .flex_1()
                                .text_sm()
                                .child(t!("versions.delete_question").into_owned())
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .whitespace_normal()
                                        .child(t!(note).into_owned()),
                                ),
                        )
                        .child(
                            Button::new(("cancel-delete", index))
                                .outline()
                                .small()
                                .label(t!("action.cancel").into_owned())
                                .disabled(self.restoring)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.confirming = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("confirm-delete", index))
                                .danger()
                                .small()
                                .label(t!("versions.delete").into_owned())
                                .loading(self.restoring)
                                .disabled(self.restoring)
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.delete(version.clone(), window, cx);
                                })),
                        ),
                )
            })
            .when(confirming == Some(Confirm::Restore), |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .child(t!("versions.restore_question").into_owned()),
                        )
                        .child(
                            Button::new(("cancel-restore", index))
                                .outline()
                                .small()
                                .label(t!("action.cancel").into_owned())
                                .disabled(self.restoring)
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.confirming = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("confirm-restore", index))
                                .primary()
                                .small()
                                .label(t!("versions.restore").into_owned())
                                .loading(self.restoring)
                                .disabled(self.restoring)
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.restore(id.clone(), window, cx);
                                })),
                        ),
                )
            })
    }
}

impl Render for VersionsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let status = self
            .versioning
            .as_deref()
            .map(|status| t!(versioning_key(status)).into_owned());
        let rows: Vec<AnyElement> = self
            .versions
            .iter()
            .enumerate()
            .map(|(index, version)| self.render_version(index, version, cx).into_any_element())
            .collect();
        let empty = rows.is_empty() && !self.loading && self.error.is_none();
        let next = self.next.clone();
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
            .when_some(status, |this, status| {
                this.child(div().text_sm().child(status))
            })
            .child(
                v_flex()
                    .w_full()
                    .max_h(rems(22.).to_pixels(window.rem_size()))
                    .overflow_y_scrollbar()
                    .children(rows)
                    .when(empty, |this| {
                        this.child(
                            div()
                                .py_4()
                                .text_sm()
                                .text_color(muted)
                                .child(t!("versions.none").into_owned()),
                        )
                    })
                    .when(self.loading, |this| {
                        this.child(
                            div()
                                .py_4()
                                .text_sm()
                                .text_color(muted)
                                .child(t!("versions.loading").into_owned()),
                        )
                    })
                    .when_some(next, |this, next| {
                        this.child(
                            h_flex().py_2().child(
                                Button::new("more-versions")
                                    .ghost()
                                    .small()
                                    .label(t!("action.load_more").into_owned())
                                    .disabled(self.loading)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if !view.loading {
                                            view.load(Some(next.clone()), cx);
                                        }
                                    })),
                            ),
                        )
                    }),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(ErrorDetails::new("versions-error", error).danger())
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("reload-versions")
                            .ghost()
                            .label(t!("action.refresh").into_owned())
                            .disabled(self.loading || self.restoring)
                            .on_click(cx.listener(|view, _, _, cx| view.reload(cx))),
                    )
                    .child(
                        Button::new("close-versions")
                            .outline()
                            .label(t!("action.close").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}
