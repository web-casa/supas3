// SPDX-License-Identifier: AGPL-3.0-or-later
//! The details panel beside the panes: what the active pane's selection is,
//! and for an object what the service reports about it.

use crate::app_state::AppBackend;
use crate::details::Subject;
use crate::entries_table::RowAction;
use crate::error_details::ErrorDetails;
use crate::listing::{format_modified, format_size, Entry};
use crate::location::Location;
use crate::object_actions::{
    access_of, needs_restore, Access, Grant, ObjectRef, CONTENT_TYPE_LIMIT,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::time::Duration;
use supas3_core::commands::operations::{ObjectMetadata, ObjectPermissions};

/// How long the selection must stay on an object before it is asked about,
/// so moving through a list does not send a request per row.
const SETTLE: Duration = Duration::from_millis(300);

/// A connection as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub name: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// IDrive e2: its default ACL has an owner without an ID, and it
    /// refuses changes to object ACLs.
    pub idrive_e2: bool,
}

pub enum DetailsEvent {
    Close,
    /// Do this with the row, read from the location.
    Row(RowAction, Location, Entry),
    /// Download the row, read from the location.
    Download(Location, Entry),
    /// Change the Content-Type of the object, which has the ETag.
    ChangeContentType {
        object: ObjectRef,
        name: String,
        current: Option<String>,
        etag: String,
    },
    /// Give the object this ACL; it now reads as `current`, and has the
    /// ETag, if one was read.
    ChangeAccess {
        object: ObjectRef,
        to: Access,
        current: Access,
        etag: Option<String>,
    },
}

/// An object's ACL as read for the panel.
enum Permissions {
    None,
    Loading,
    Loaded {
        status: String,
        message: Option<String>,
        access: Access,
    },
    Failed(String),
}

impl Permissions {
    fn from(permissions: ObjectPermissions, idrive_e2: bool) -> Self {
        let grants: Vec<Grant> = permissions
            .grants
            .iter()
            .map(|grant| Grant {
                grantee_type: grant.grantee_type.as_deref(),
                id: grant.id.as_deref(),
                uri: grant.uri.as_deref(),
                permission: grant.permission.as_deref(),
            })
            .collect();
        let access = access_of(permissions.owner_id.as_deref(), &grants, idrive_e2);
        Self::Loaded {
            status: permissions.status,
            message: permissions.message,
            access,
        }
    }
}

enum Metadata {
    None,
    Loading,
    Loaded(ObjectMetadata),
    Failed(String),
}

pub struct DetailsPanel {
    /// Where the subject's rows were read from.
    source: Option<Location>,
    subject: Option<Subject>,
    connection: Option<ConnectionInfo>,
    metadata: Metadata,
    permissions: Permissions,
    /// Counts subjects shown; an answer about an earlier one is dropped.
    generation: u64,
}

impl EventEmitter<DetailsEvent> for DetailsPanel {}

impl DetailsPanel {
    pub fn new() -> Self {
        Self {
            source: None,
            subject: None,
            connection: None,
            metadata: Metadata::None,
            permissions: Permissions::None,
            generation: 0,
        }
    }

    /// Describes `subject`, read from `source`. Showing the same subject
    /// again changes nothing; for an object, its properties are asked for
    /// once the selection has settled.
    pub fn show(
        &mut self,
        source: Location,
        subject: Subject,
        connection: Option<ConnectionInfo>,
        cx: &mut Context<Self>,
    ) {
        if self.subject.as_ref() == Some(&subject)
            && self.source.as_ref() == Some(&source)
            && self.connection == connection
        {
            return;
        }
        self.source = Some(source);
        self.subject = Some(subject);
        self.connection = connection;
        self.load(SETTLE, cx);
    }

    /// Reads the object shown again, after it was changed from here.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(Duration::ZERO, cx);
    }

    /// An access change is under way: its choices go until the panel is
    /// read again, so it cannot be asked for twice at once.
    pub fn access_changing(&mut self, cx: &mut Context<Self>) {
        self.permissions = Permissions::Loading;
        cx.notify();
    }

    fn load(&mut self, settle: Duration, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.metadata = Metadata::None;
        self.permissions = Permissions::None;
        if let Some(Subject::Object { object, .. }) = &self.subject {
            self.metadata = Metadata::Loading;
            self.permissions = Permissions::Loading;
            let object = object.clone();
            let idrive_e2 = self
                .connection
                .as_ref()
                .is_some_and(|connection| connection.idrive_e2);
            if let Some(requests) = AppBackend::requests(cx) {
                cx.spawn(async move |panel, cx| {
                    cx.background_executor().timer(settle).await;
                    let current = panel
                        .read_with(cx, |panel, _| panel.generation == generation)
                        .unwrap_or(false);
                    if !current {
                        return;
                    }
                    let result = requests.object_properties(object.clone()).await;
                    let _ = panel.update(cx, |panel, cx| {
                        if panel.generation == generation {
                            panel.metadata = match result {
                                Ok(metadata) => Metadata::Loaded(metadata),
                                Err(error) => Metadata::Failed(error),
                            };
                            cx.notify();
                        }
                    });
                    let result = requests.object_permissions(object).await;
                    let _ = panel.update(cx, |panel, cx| {
                        if panel.generation == generation {
                            panel.permissions = match result {
                                Ok(permissions) => Permissions::from(permissions, idrive_e2),
                                Err(error) => Permissions::Failed(error),
                            };
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    /// The Content-Type line's change button, for an object read with an
    /// ETag and small enough to be copied in one request.
    fn content_type_button(
        &self,
        object: &ObjectRef,
        name: &str,
        metadata: &ObjectMetadata,
        cx: &mut Context<Self>,
    ) -> Option<Button> {
        let etag = metadata.e_tag.clone()?;
        let size = u64::try_from(metadata.size).ok()?;
        let button = Button::new("details-content-type")
            .small()
            .outline()
            .label(t!("content_type.change").into_owned());
        let refusal = if size > CONTENT_TYPE_LIMIT {
            Some(t!("content_type.too_large"))
        } else if needs_restore(metadata.storage_class.as_deref()) {
            Some(t!("content_type.archived"))
        } else {
            None
        };
        if let Some(refusal) = refusal {
            return Some(button.disabled(true).tooltip(refusal.into_owned()));
        }
        let (object, name, current) = (
            object.clone(),
            name.to_string(),
            metadata.content_type.clone(),
        );
        Some(button.on_click(cx.listener(move |_, _, _, cx| {
            cx.emit(DetailsEvent::ChangeContentType {
                object: object.clone(),
                name: name.clone(),
                current: current.clone(),
                etag: etag.clone(),
            });
        })))
    }

    /// Who may read the object, and the choice to change it.
    fn render_access(&self, object: &ObjectRef, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let heading = div()
            .text_sm()
            .font_semibold()
            .child(t!("access.title").into_owned());
        let body = match &self.permissions {
            Permissions::None => return div().into_any_element(),
            Permissions::Loading => div()
                .text_sm()
                .text_color(muted)
                .child(t!("view.loading").into_owned())
                .into_any_element(),
            Permissions::Failed(error) => {
                ErrorDetails::new("access-error", error.clone()).into_any_element()
            }
            Permissions::Loaded {
                status, message, ..
            } if status != "available" => {
                let key = match status.as_str() {
                    "unsupported" => "access.unsupported",
                    "access_denied" => "access.denied",
                    _ => "access.unknown",
                };
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(t!(key).into_owned()),
                    )
                    .when_some(message.clone(), |this, message| {
                        this.child(div().text_xs().text_color(muted).child(message))
                    })
                    .into_any_element()
            }
            Permissions::Loaded { access, .. } => {
                let current = *access;
                let label = match current {
                    Access::Private => t!("access.private"),
                    Access::PublicRead => t!("access.public_read"),
                    Access::Custom => t!("access.custom"),
                };
                let etag = match &self.metadata {
                    Metadata::Loaded(metadata) => metadata.e_tag.clone(),
                    _ => None,
                };
                let choice = |id: &'static str, to: Access, label: String| {
                    let (object, etag) = (object.clone(), etag.clone());
                    Button::new(id)
                        .small()
                        .outline()
                        .label(label)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(DetailsEvent::ChangeAccess {
                                object: object.clone(),
                                to,
                                current,
                                etag: etag.clone(),
                            });
                        }))
                };
                let changes = self
                    .connection
                    .as_ref()
                    .is_some_and(|connection| !connection.idrive_e2);
                v_flex()
                    .gap_2()
                    .child(div().text_sm().child(label.into_owned()))
                    .when(!changes, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("access.read_only").into_owned()),
                        )
                    })
                    .when(changes, |this| {
                        this.child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .when(current != Access::Private, |this| {
                                    this.child(choice(
                                        "access-private",
                                        Access::Private,
                                        t!("access.make_private").into_owned(),
                                    ))
                                })
                                .when(current != Access::PublicRead, |this| {
                                    this.child(choice(
                                        "access-public",
                                        Access::PublicRead,
                                        t!("access.make_public").into_owned(),
                                    ))
                                }),
                        )
                    })
                    .into_any_element()
            }
        };
        v_flex()
            .gap_2()
            .child(heading)
            .child(body)
            .into_any_element()
    }

    fn row_button(
        &self,
        id: &'static str,
        label: String,
        action: RowAction,
        entry: &Entry,
        cx: &mut Context<Self>,
    ) -> Button {
        let (source, entry) = (self.source.clone(), entry.clone());
        Button::new(id)
            .small()
            .outline()
            .label(label)
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(source) = source.clone() {
                    cx.emit(DetailsEvent::Row(action, source, entry.clone()));
                }
            }))
    }
}

/// A size with the exact byte count where the short form rounds.
fn exact_size(size: u64) -> String {
    let short = format_size(size);
    if short == format!("{size} B") {
        short
    } else {
        format!("{short} ({size} B)")
    }
}

impl Render for DetailsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let label_width = rems(6.).to_pixels(window.rem_size());
        let unknown = || t!("properties.unknown").into_owned();
        let list = || DescriptionList::new().columns(1).label_width(label_width);
        let note = |text: String| div().text_sm().text_color(muted).child(text);

        let body = match self.subject.clone() {
            None => note(t!("details.nothing").into_owned()).into_any_element(),
            Some(Subject::Location { location, loaded }) => {
                let place = match &location {
                    Location::Local(path) => path.display().to_string(),
                    Location::Buckets { .. } => self
                        .connection
                        .as_ref()
                        .map(|connection| connection.name.clone())
                        .unwrap_or_default(),
                    Location::Objects { bucket, prefix, .. } => format!("{bucket}/{prefix}"),
                };
                let mut items = list().item(t!("details.location").into_owned(), place, 1);
                if let (Location::Buckets { .. }, Some(connection)) = (&location, &self.connection)
                {
                    items = items
                        .item(
                            t!("details.endpoint").into_owned(),
                            connection.endpoint.clone().unwrap_or_else(unknown),
                            1,
                        )
                        .item(
                            t!("details.region").into_owned(),
                            connection.region.clone().unwrap_or_else(unknown),
                            1,
                        );
                }
                items
                    .item(t!("details.loaded").into_owned(), loaded.to_string(), 1)
                    .into_any_element()
            }
            Some(Subject::Many {
                count,
                size,
                unsized_rows,
            }) => {
                let mut items = list()
                    .item(t!("details.selected").into_owned(), count.to_string(), 1)
                    .item(t!("properties.size").into_owned(), exact_size(size), 1);
                if unsized_rows > 0 {
                    items = items.item(
                        t!("details.unsized").into_owned(),
                        unsized_rows.to_string(),
                        1,
                    );
                }
                items.into_any_element()
            }
            Some(Subject::Bucket { bucket, .. }) => {
                let mut items = list().item(t!("details.bucket").into_owned(), bucket, 1);
                if let Some(connection) = &self.connection {
                    items = items
                        .item(
                            t!("details.connection").into_owned(),
                            connection.name.clone(),
                            1,
                        )
                        .item(
                            t!("details.endpoint").into_owned(),
                            connection.endpoint.clone().unwrap_or_else(unknown),
                            1,
                        )
                        .item(
                            t!("details.region").into_owned(),
                            connection.region.clone().unwrap_or_else(unknown),
                            1,
                        );
                }
                items.into_any_element()
            }
            Some(Subject::Folder { location, entry }) => {
                let place = match &location {
                    Location::Local(path) => path.display().to_string(),
                    Location::Objects { bucket, prefix, .. } => format!("{bucket}/{prefix}"),
                    Location::Buckets { .. } => entry.name.clone(),
                };
                list()
                    .item(t!("details.name").into_owned(), entry.name.clone(), 1)
                    .item(t!("details.location").into_owned(), place, 1)
                    .into_any_element()
            }
            Some(Subject::LocalFile { path, entry }) => list()
                .item(t!("details.name").into_owned(), entry.name.clone(), 1)
                .item(
                    t!("properties.size").into_owned(),
                    entry.size.map(exact_size).unwrap_or_else(unknown),
                    1,
                )
                .item(
                    t!("properties.modified").into_owned(),
                    entry
                        .modified
                        .as_deref()
                        .map(format_modified)
                        .unwrap_or_else(unknown),
                    1,
                )
                .item(
                    t!("details.path").into_owned(),
                    path.display().to_string(),
                    1,
                )
                .into_any_element(),
            Some(Subject::Object { object, entry }) => {
                let reported = match &self.metadata {
                    Metadata::Loaded(metadata) => {
                        let mut items = list()
                            .item(t!("properties.key").into_owned(), object.key.clone(), 1)
                            .item(
                                t!("properties.size").into_owned(),
                                u64::try_from(metadata.size)
                                    .map(exact_size)
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
                            items = items.item(format!("x-amz-meta-{name}"), value.clone(), 1);
                        }
                        let change = self.content_type_button(&object, &entry.name, metadata, cx);
                        v_flex()
                            .gap_2()
                            .child(items)
                            .when_some(change, |this, change| this.child(h_flex().child(change)))
                            .into_any_element()
                    }
                    Metadata::Failed(error) => {
                        ErrorDetails::new("details-error", error.clone()).into_any_element()
                    }
                    Metadata::Loading | Metadata::None => list()
                        .item(t!("properties.key").into_owned(), object.key.clone(), 1)
                        .item(
                            t!("properties.size").into_owned(),
                            entry.size.map(exact_size).unwrap_or_else(unknown),
                            1,
                        )
                        .into_any_element(),
                };
                let (download_source, download_entry) = (self.source.clone(), entry.clone());
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .child(self.row_button(
                                "details-open",
                                t!("action.open").into_owned(),
                                RowAction::Open,
                                &entry,
                                cx,
                            ))
                            .child(self.row_button(
                                "details-share",
                                t!("action.share_ellipsis").into_owned(),
                                RowAction::Share,
                                &entry,
                                cx,
                            ))
                            .child(self.row_button(
                                "details-versions",
                                t!("action.versions_ellipsis").into_owned(),
                                RowAction::Versions,
                                &entry,
                                cx,
                            ))
                            .child(
                                Button::new("details-download")
                                    .small()
                                    .outline()
                                    .label(t!("details.download").into_owned())
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        if let Some(source) = download_source.clone() {
                                            cx.emit(DetailsEvent::Download(
                                                source,
                                                download_entry.clone(),
                                            ));
                                        }
                                    })),
                            ),
                    )
                    .child(reported)
                    .when(matches!(self.metadata, Metadata::Loading), |this| {
                        this.child(note(t!("view.loading").into_owned()))
                    })
                    .when(!matches!(self.metadata, Metadata::Loading), |this| {
                        this.child(self.render_access(&object, cx))
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .min_w_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(t!("details.title").into_owned()),
                    )
                    .child(
                        Button::new("details-close")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t!("details.hide").into_owned())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DetailsEvent::Close))),
                    ),
            )
            .child(
                div()
                    .id("details-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .child(body),
            )
    }
}
