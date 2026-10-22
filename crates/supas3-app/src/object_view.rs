// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shows one file: text that can be edited when it is an object in a bucket,
//! or an image. Text opens in a tab of a pane; images in a dialog.

use crate::app_state::AppBackend;
use crate::listing::format_size;
use crate::object_actions::{
    brief_error, image_container_is_supported, image_fits, opening, FileRef, ImageKind, Opening,
    Zoom, TEXT_LIMIT,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

/// Long lines are wrapped only in text up to this size. Wrapping is worked
/// out for the whole text at once, and above about a megabyte that holds the
/// window for many seconds; without it, 8 MiB stays quick to edit.
const WRAP_LIMIT: u64 = 256 * 1024;

pub enum ObjectViewEvent {
    /// The object was saved; lists showing it are out of date.
    Saved,
    /// A save did not go through; the edits are still here.
    SaveFailed,
    /// The view in a tab asks to be closed; nothing is lost by it.
    CloseRequested,
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
    Image {
        image: Arc<Image>,
        /// Width and height in pixels.
        size: (u32, u32),
    },
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
    /// Shown in a tab rather than a dialog.
    in_tab: bool,
    /// The view itself, for the keyboard when it shows no text.
    focus: FocusHandle,
    zoom: Zoom,
    /// The size of the area an image is shown in, as last laid out.
    viewport: Rc<Cell<(f32, f32)>>,
}

impl EventEmitter<ObjectViewEvent> for ObjectView {}

impl ObjectView {
    pub fn new(
        file: FileRef,
        name: &str,
        size: Option<u64>,
        guarded: bool,
        in_tab: bool,
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
                Self::load_image(file.clone(), kind, window, cx);
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
            in_tab,
            focus: cx.focus_handle(),
            zoom: Zoom::Fit,
            viewport: Rc::new(Cell::new((0.0, 0.0))),
        }
    }

    pub fn file(&self) -> &FileRef {
        &self.file
    }

    /// Puts the keyboard in the text, or on the view itself while there is
    /// none, so moving through the window with Tab never stops here.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        match &self.body {
            Body::Text { input, .. } => {
                let handle = input.focus_handle(cx);
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
    }

    /// Whether the keyboard is anywhere in the view.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    pub fn is_saving(&self) -> bool {
        self.saving
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
                        let input = cx.new(|cx| {
                            TextareaState::new(window, cx)
                                .soft_wrap(saved.len() as u64 <= WRAP_LIMIT)
                                .default_value(saved.clone())
                        });
                        // In a tab the text takes the keyboard when it arrives.
                        if view.in_tab {
                            let handle = input.focus_handle(cx);
                            window.focus(&handle, cx);
                        }
                        let changes = cx.subscribe(&input, |view, _, event: &InputEvent, cx| {
                            if matches!(event, InputEvent::Change) {
                                view.save_error = None;
                                // Typing on means the question is moot.
                                view.confirming_discard = false;
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

    fn load_image(file: FileRef, kind: ImageKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn_in(window, async move |view, cx| {
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
                            Body::Image {
                                image: Arc::new(Image::from_bytes(format, bytes)),
                                size: (width, height),
                            }
                        }
                        (_, Some(_)) => Body::Note(t!("view.image_too_many_pixels").into_owned()),
                        (_, None) => Body::Note(t!("view.image_unreadable").into_owned()),
                    }
                }
                Err(error) => Body::Failed(error),
            };
            let _ = view.update_in(cx, |view, window, cx| {
                // The zoom keys work once the picture is there.
                if matches!(body, Body::Image { .. }) && !view.in_tab {
                    window.focus(&view.focus, cx);
                }
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
    pub fn is_editable(&self) -> bool {
        matches!(
            (&self.file, &self.body),
            (FileRef::Remote(_), Body::Text { etag: Some(_), .. })
        )
    }

    /// Saves the edits. Text over the size limit is not saved: it could not
    /// be opened again here.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving || !self.is_dirty(cx) {
            return;
        }
        if let Body::Text { input, .. } = &self.body {
            if input.read(cx).value().len() as u64 > TEXT_LIMIT {
                self.save_error = Some(
                    t!("view.too_large_to_save", limit = format_size(TEXT_LIMIT)).into_owned(),
                );
                cx.emit(ObjectViewEvent::SaveFailed);
                cx.notify();
                return;
            }
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
            cx.emit(ObjectViewEvent::SaveFailed);
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
                    Err(error) => {
                        view.save_error = Some(error);
                        cx.emit(ObjectViewEvent::SaveFailed);
                    }
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

    /// Closes the view, asking first when edits would be lost. A save in
    /// flight finishes first: its outcome has to be shown.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if !self.is_dirty(cx) {
            self.dismiss(window, cx);
        } else if !self.confirming_discard {
            // The edits go only by answering the question; asking again,
            // for instance by a second click, changes nothing.
            self.confirming_discard = true;
            cx.notify();
        }
    }

    /// Goes away without asking: the user decided.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_tab {
            cx.emit(ObjectViewEvent::CloseRequested);
        } else {
            window.close_dialog(cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.key == "s" && keystroke.modifiers.secondary() && self.is_editable() {
            self.save(cx);
            cx.stop_propagation();
            return;
        }
        if let Body::Image { size, .. } = self.body {
            if !keystroke.modifiers.secondary() {
                return;
            }
            let fit = self.fit_scale(size);
            let zoom = match keystroke.key.as_str() {
                "=" | "+" if self.zoom.can_zoom_in(fit) => self.zoom.zoomed_in(fit),
                "-" if self.zoom.can_zoom_out(fit) => self.zoom.zoomed_out(fit),
                "=" | "+" | "-" => self.zoom,
                "0" => Zoom::Fit,
                _ => return,
            };
            self.set_zoom(zoom, cx);
            cx.stop_propagation();
        }
    }

    /// The scale that fits an image into the area last laid out for it.
    fn fit_scale(&self, size: (u32, u32)) -> f32 {
        match self.viewport.get() {
            (width, height) if width > 0.0 && height > 0.0 => {
                Zoom::fit_scale(size, (width, height))
            }
            _ => 1.0,
        }
    }

    fn set_zoom(&mut self, zoom: Zoom, cx: &mut Context<Self>) {
        self.zoom = zoom;
        cx.notify();
    }

    fn render_image(
        &self,
        image: &Arc<Image>,
        size: (u32, u32),
        height: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fit = self.fit_scale(size);
        let zoom = self.zoom;
        // Centred in a frame at least as large as the area: when the
        // picture is larger, the frame is its size, and the area scrolls
        // over all of it.
        let picture = match zoom {
            Zoom::Fit => h_flex().size_full().justify_center().child(
                img(image.clone())
                    .max_w_full()
                    .max_h_full()
                    .object_fit(ObjectFit::ScaleDown),
            ),
            Zoom::Scale(scale) => {
                let (width, height) = (size.0 as f32 * scale, size.1 as f32 * scale);
                let (area_width, area_height) = self.viewport.get();
                h_flex()
                    .flex_shrink_0()
                    .w(px(width.max(area_width)))
                    .h(px(height.max(area_height)))
                    .justify_center()
                    .child(
                        img(image.clone())
                            .flex_shrink_0()
                            .w(px(width))
                            .h(px(height)),
                    )
            }
        };
        let viewport = self.viewport.clone();
        let view = cx.entity_id();
        let tools = h_flex()
            .gap_1()
            .items_center()
            .child(
                Button::new("zoom-out")
                    .ghost()
                    .small()
                    .icon(IconName::Minus)
                    .tooltip(t!("view.zoom_out").into_owned())
                    .disabled(!zoom.can_zoom_out(fit))
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.set_zoom(zoom.zoomed_out(fit), cx)),
                    ),
            )
            .child(
                div()
                    .w(rems(3.5))
                    .text_center()
                    .text_sm()
                    .child(format!("{}%", zoom.percent(fit))),
            )
            .child(
                Button::new("zoom-in")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .tooltip(t!("view.zoom_in").into_owned())
                    .disabled(!zoom.can_zoom_in(fit))
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.set_zoom(zoom.zoomed_in(fit), cx)),
                    ),
            )
            .child(
                Button::new("zoom-fit")
                    .ghost()
                    .small()
                    .label(t!("view.zoom_fit").into_owned())
                    .selected(zoom == Zoom::Fit)
                    .on_click(cx.listener(|view, _, _, cx| view.set_zoom(Zoom::Fit, cx))),
            )
            .child(
                Button::new("zoom-actual")
                    .ghost()
                    .small()
                    .label(t!("view.zoom_actual").into_owned())
                    .selected(zoom == Zoom::Scale(1.0))
                    .on_click(cx.listener(|view, _, _, cx| view.set_zoom(Zoom::Scale(1.0), cx))),
            )
            .child(
                div()
                    .flex_1()
                    .text_right()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} × {}", size.0, size.1)),
            );
        v_flex()
            .w_full()
            .gap_2()
            .child(tools)
            .child(
                div()
                    .id("image-area")
                    .relative()
                    .h(height)
                    .w_full()
                    .overflow_scroll()
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let size =
                                    (f32::from(bounds.size.width), f32::from(bounds.size.height));
                                // The fitted scale shown depends on it: draw
                                // again with the size now known.
                                if viewport.replace(size) != size {
                                    cx.notify(view);
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(picture),
            )
            .into_any_element()
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
            Body::Image { image, size } => self.render_image(image, *size, height, cx),
            Body::Text { input, .. } => Textarea::new(input)
                .when(self.in_tab, |this| this.flex_1().min_h_0())
                .when(!self.in_tab, |this| this.h(height))
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
            .when(self.in_tab, |this| this.size_full().p_3())
            .track_focus(&self.focus)
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
                                    view.dismiss(window, cx);
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
