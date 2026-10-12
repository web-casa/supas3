// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shows one file: text that can be edited when it is an object in a bucket,
//! or an image. Lives inside a dialog the workspace opens.

use crate::app_state::AppBackend;
use crate::listing::format_size;
use crate::object_actions::{
    brief_error, image_container_is_supported, image_fits, opening, FileRef, ImageKind, Opening,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::sync::Arc;

pub enum ObjectViewEvent {
    /// The object was saved; lists showing it are out of date.
    Saved,
}

enum Body {
    Loading,
    Text {
        input: Entity<TextareaState>,
        /// The content as last read or saved; edits are compared with it.
        saved: SharedString,
        /// What the object must still have for a save to go through.
        etag: Option<String>,
        _changes: Subscription,
    },
    Image(Arc<Image>),
    /// Nothing is shown, for a reason that is not an error.
    Note(String),
    Failed(String),
}

pub struct ObjectView {
    file: FileRef,
    body: Body,
    /// Whether the service itself refuses a save over a changed object.
    guarded: bool,
    saving: bool,
    save_error: Option<String>,
    /// The user asked to close with unsaved edits and has not yet answered.
    confirming_discard: bool,
}

impl EventEmitter<ObjectViewEvent> for ObjectView {}

impl ObjectView {
    pub fn new(
        file: FileRef,
        name: &str,
        size: Option<u64>,
        guarded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let body = match opening(name, size) {
            Opening::Unsupported => Body::Note(t!("view.unsupported").into_owned()),
            Opening::TooLarge(limit) => {
                Body::Note(t!("view.too_large", limit = format_size(limit)).into_owned())
            }
            Opening::Text => {
                Self::load_text(file.clone(), window, cx);
                Body::Loading
            }
            Opening::Image(kind) => {
                Self::load_image(file.clone(), kind, cx);
                Body::Loading
            }
        };
        Self {
            file,
            body,
            guarded,
            saving: false,
            save_error: None,
            confirming_discard: false,
        }
    }

    fn load_text(file: FileRef, window: &mut Window, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn_in(window, async move |view, cx| {
            let result = requests.read_text(file).await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.body = match result {
                    Ok(text) => {
                        let saved: SharedString = text.content.into();
                        let input = cx
                            .new(|cx| TextareaState::new(window, cx).default_value(saved.clone()));
                        let changes = cx.subscribe(&input, |view, _, event: &InputEvent, cx| {
                            if matches!(event, InputEvent::Change) {
                                view.save_error = None;
                                cx.notify();
                            }
                        });
                        Body::Text {
                            input,
                            saved,
                            etag: text.etag,
                            _changes: changes,
                        }
                    }
                    Err(error) => Body::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn load_image(file: FileRef, kind: ImageKind, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn(async move |view, cx| {
            let result = requests.read_image(file).await;
            let body = match result {
                Ok(bytes) => {
                    // Look at the header only; decoding happens when drawn.
                    let dimensions = cx
                        .background_executor()
                        .spawn(async move {
                            let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
                                .with_guessed_format()
                                .ok()
                                .and_then(|reader| reader.into_dimensions().ok());
                            (bytes, size)
                        })
                        .await;
                    match dimensions {
                        (bytes, _) if !image_container_is_supported(kind, &bytes) => {
                            Body::Note(t!("view.image_form_unsupported").into_owned())
                        }
                        (bytes, Some((width, height))) if image_fits(width, height) => {
                            let format = match kind {
                                ImageKind::Png => ImageFormat::Png,
                                ImageKind::Jpeg => ImageFormat::Jpeg,
                                ImageKind::Webp => ImageFormat::Webp,
                            };
                            Body::Image(Arc::new(Image::from_bytes(format, bytes)))
                        }
                        (_, Some(_)) => Body::Note(t!("view.image_too_many_pixels").into_owned()),
                        (_, None) => Body::Note(t!("view.image_unreadable").into_owned()),
                    }
                }
                Err(error) => Body::Failed(error),
            };
            let _ = view.update(cx, |view, cx| {
                view.body = body;
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether the text has edits that were not saved.
    pub fn is_dirty(&self, cx: &App) -> bool {
        match &self.body {
            Body::Text { input, saved, .. } => input.read(cx).value() != *saved,
            _ => false,
        }
    }

    /// Whether the shown text can be saved back: an object whose ETag is
    /// known. Local files are shown read-only.
    fn is_editable(&self) -> bool {
        matches!(
            (&self.file, &self.body),
            (FileRef::Remote(_), Body::Text { etag: Some(_), .. })
        )
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving || !self.is_dirty(cx) {
            return;
        }
        let (FileRef::Remote(object), Body::Text { input, etag, .. }) = (&self.file, &self.body)
        else {
            return;
        };
        let Some(etag) = etag.clone() else {
            return;
        };
        let Some(requests) = AppBackend::requests(cx) else {
            self.save_error = Some(t!("error.backend_stopped").into_owned());
            cx.notify();
            return;
        };
        let content = input.read(cx).value();
        let object = object.clone();
        self.saving = true;
        self.save_error = None;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = requests.save_text(object, content.to_string(), etag).await;
            let _ = view.update(cx, |view, cx| {
                view.saving = false;
                match result {
                    Ok(new_etag) => {
                        if let Body::Text { saved, etag, .. } = &mut view.body {
                            // Edits typed while saving stay unsaved.
                            *saved = content;
                            *etag = new_etag;
                        }
                        cx.emit(ObjectViewEvent::Saved);
                    }
                    Err(error) => view.save_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether closing now would lose something: unsaved edits, or the answer
    /// of a save that is on its way and cannot be taken back.
    pub fn holds_work(&self, cx: &App) -> bool {
        self.saving || self.is_dirty(cx)
    }

    /// Closes the dialog, asking first when edits would be lost. A save in
    /// flight finishes first: its outcome has to be shown.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.is_dirty(cx) && !self.confirming_discard {
            self.confirming_discard = true;
            cx.notify();
        } else {
            window.close_dialog(cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.key == "s" && keystroke.modifiers.secondary() && self.is_editable() {
            self.save(cx);
            cx.stop_propagation();
        }
    }

    fn render_note(&self, text: String, danger: bool, cx: &App) -> AnyElement {
        div()
            .py_6()
            .text_sm()
            .text_color(if danger {
                cx.theme().danger
            } else {
                cx.theme().muted_foreground
            })
            .child(text)
            .into_any_element()
    }
}

impl Render for ObjectView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.is_dirty(cx);
        let editable = self.is_editable();
        let height = rems(26.).to_pixels(window.rem_size());
        let body = match &self.body {
            Body::Loading => self.render_note(t!("view.loading").into_owned(), false, cx),
            Body::Note(text) => self.render_note(text.clone(), false, cx),
            Body::Failed(error) => self.render_note(
                t!("view.failed", detail = brief_error(error)).into_owned(),
                true,
                cx,
            ),
            Body::Image(image) => h_flex()
                .h(height)
                .w_full()
                .justify_center()
                .child(
                    img(image.clone())
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::ScaleDown),
                )
                .into_any_element(),
            Body::Text { input, .. } => Textarea::new(input)
                .h(height)
                .readonly(!editable || self.saving)
                .into_any_element(),
        };
        // What the user should know about saving, in one line under the text.
        let hint = match (&self.file, &self.body) {
            (FileRef::Local(_), Body::Text { .. }) => Some(t!("view.local_read_only")),
            (FileRef::Remote(_), Body::Text { etag: None, .. }) => Some(t!("view.no_etag")),
            (FileRef::Remote(_), Body::Text { .. }) if !self.guarded => {
                Some(t!("view.save_not_guarded"))
            }
            _ => None,
        };
        v_flex()
            .w_full()
            .gap_3()
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(self.file.display()),
            )
            .child(body)
            .when_some(hint, |this, hint| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hint.into_owned()),
                )
            })
            .when_some(self.save_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(t!("view.save_failed", detail = brief_error(&error)).into_owned()),
                )
            })
            .child(if self.confirming_discard {
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .child(t!("view.discard_question").into_owned()),
                    )
                    .child(
                        Button::new("keep-editing")
                            .outline()
                            .label(t!("view.keep_editing").into_owned())
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.confirming_discard = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("discard-edits")
                            .danger()
                            .label(t!("view.discard").into_owned())
                            .disabled(self.saving)
                            .on_click(cx.listener(|view, _, window, cx| {
                                if !view.saving {
                                    window.close_dialog(cx);
                                }
                            })),
                    )
            } else {
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(dirty, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("view.unsaved").into_owned()),
                        )
                    })
                    .child(
                        Button::new("close-view")
                            .outline()
                            .label(t!("action.close").into_owned())
                            .disabled(self.saving)
                            .on_click(cx.listener(|view, _, window, cx| view.close(window, cx))),
                    )
                    .when(editable, |this| {
                        this.child(
                            Button::new("save-text")
                                .primary()
                                .label(t!("action.save").into_owned())
                                .loading(self.saving)
                                .disabled(!dirty || self.saving)
                                .on_click(cx.listener(|view, _, _, cx| view.save(cx))),
                        )
                    })
            })
    }
}
