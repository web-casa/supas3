// SPDX-License-Identifier: AGPL-3.0-or-later
//! The main window: connections on the side, two panes of tabs beside them.

use crate::app_jobs::{AppJobs, JobContext, JobError, JobLabel};
use crate::app_state::{AppBackend, Connections};
use crate::backend::{default_config_dir, Requests};
use crate::bucket_views::{BucketEvent, CreateBucketDialog, DeleteBucketDialog, PolicyDialog};
use crate::conflicts::{conflicts, resolve, Choice, Existing};
use crate::connection::Service;
use crate::connection_form::{ConnectionForm, ConnectionFormEvent};
use crate::content_type_view::{ContentTypeDialog, ContentTypeEvent};
use crate::details_view::{ConnectionInfo, DetailsEvent, DetailsPanel};
use crate::entries_table::{DraggedEntries, RowAction};
use crate::file_ops::{
    check_local_name, check_object_name, folder_moves, stem_range, unique_name, FOLDER_RENAME_LIMIT,
};
use crate::folder_access::FOLDER_ACCESS_LIMIT;
use crate::i18n;
use crate::import_view::{ImportDialog, ImportEvent};
use crate::listing::Entry;
use crate::listing::EntryKind;
use crate::local_copy;
use crate::location::Location;
use crate::metadata_view::{MetadataDialog, MetadataEvent};
use crate::name_dialog::{NameDialog, NameDialogEvent};
use crate::object_actions::{
    access_of, address_of, brief_error, file_of, link_may_expire_early, plain_address_is_known,
    sync_pair, Access, FileRef, Grant, ObjectRef,
};
use crate::object_view::{ObjectView, ObjectViewEvent};
use crate::pane::{Pane, PaneEvent};
use crate::places::{full_name, short_name, Layout, PaneLayout, Places};
use crate::search_view::{SearchView, SearchViewEvent};
use crate::settings::{Appearance, Language, Settings};
use crate::share_view::ShareView;
use crate::sync_view::SyncView;
use crate::task_center::{TaskCenter, TaskCenterEvent};
use crate::tasks::EventQueue;
use crate::tasks::{TaskKind, TaskState};
use crate::transfer_plan::{
    plan, plan_deletion, plan_drop, plan_local_deletion, Deletion, LocalDeletion, LocalItem, Plan,
    Replace,
};
use crate::transfers_view::{TransfersDialog, TransfersDialogEvent};
use crate::versions_view::VersionsView;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::sidebar::SidebarToggleButton;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, IconName, Sizable as _, StyledExt as _, Theme, ThemeMode,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::path::PathBuf;
use supas3_core::commands::operations::SeenProperties;
use supas3_core::credentials::Profile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

pub struct Workspace {
    connections: Entity<Connections>,
    /// Favorites, recent places and the saved layout, written to disk on
    /// every change.
    places: Entity<Places>,
    /// Whether the panes were opened from a saved layout, which the first
    /// connection must not replace.
    restored: bool,
    left: Entity<Pane>,
    right: Entity<Pane>,
    task_center: Entity<TaskCenter>,
    details: Entity<DetailsPanel>,
    /// The sidebar takes keyboard focus; the cursor is the row it is on.
    sidebar_focus: FocusHandle,
    sidebar_cursor: usize,
    /// The pane that sidebar choices open in.
    active: Side,
    settings: Settings,
    /// Kept alive while its dialog is open.
    form: Option<Entity<ConnectionForm>>,
    /// A problem that is not tied to one pane, shown above the panes.
    notice: Option<String>,
    /// Places could not be saved; said once, not on every navigation.
    places_unsaved: bool,
    /// Folder renames running as jobs, by job id: where from and where to.
    pending_renames: std::collections::HashMap<String, (Location, Location)>,
    /// Leaving waits for these saves to finish.
    leaving: Option<(Leave, std::collections::HashSet<EntityId>)>,
    /// The window the workspace is in.
    window: AnyWindowHandle,
    /// Which storage failures have been reported: the journal, saved jobs.
    storage_warned: [bool; 2],
    /// What the backend could not write, shown until the app restarts.
    storage_notice: Option<String>,
    /// Moves still running; the panes are reloaded when each ends.
    pending_moves: std::collections::HashSet<String>,
    /// Transfers retried on their own after a network failure, with how
    /// many retries led to each.
    retries: std::collections::HashMap<String, u32>,
    /// The window is too narrow for the full sidebar beside the details
    /// panel; the sidebar shows icons only, whatever the preference.
    sidebar_squeezed: bool,
    /// The user opened the squeezed sidebar anyway; lasts until the window
    /// has room again or the details panel closes. Not saved.
    sidebar_unsqueezed: bool,
    /// Counts connection reloads, so only the latest answer is applied.
    reload_generation: u64,
    _subscriptions: Vec<Subscription>,
}

/// The folder a local pane starts in.
fn home() -> Location {
    Location::Local(dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")))
}

/// Renames a folder in a bucket by moving each object under it, one at a
/// time: each object is copied, then its original deleted, before the next.
/// Runs on a job thread.
fn rename_folder(
    requests: &crate::backend::Requests,
    job: &JobContext,
    profile_id: &str,
    bucket: &str,
    from: &str,
    to: &str,
) -> Result<(), JobError> {
    let keys = requests
        .wait(requests.list_keys(profile_id.into(), bucket.into(), from.into()))
        .map_err(JobError::Failed)?;
    if keys.len() > FOLDER_RENAME_LIMIT {
        return Err(JobError::Failed(
            t!("rename.too_many", limit = FOLDER_RENAME_LIMIT).into_owned(),
        ));
    }
    // A key ending in `/` that holds data is not a folder marker; renaming
    // would turn it into an empty one, so nothing is moved.
    if let Some(odd) = keys.iter().find(|k| k.key.ends_with('/') && k.size > 0) {
        return Err(JobError::Failed(
            t!("rename.folder_object", key = odd.key.as_str()).into_owned(),
        ));
    }
    let etags: std::collections::HashMap<String, String> = keys
        .iter()
        .map(|k| (k.key.clone(), k.etag.clone().unwrap_or_default()))
        .collect();
    let keys: Vec<String> = keys.into_iter().map(|listed| listed.key).collect();
    let moves = folder_moves(&keys, from, to);
    let total = moves.len() as u64;
    job.progress(0, total);
    for (done, (old_key, new_key)) in moves.iter().enumerate() {
        if job.cancelled() {
            return Err(JobError::Cancelled);
        }
        // Folder markers are empty objects; the backend's move treats a key
        // ending in `/` as a whole folder, so they are recreated instead.
        let moved = if old_key.ends_with('/') {
            requests.wait(requests.move_folder_marker(
                profile_id.into(),
                bucket.into(),
                old_key.clone(),
                new_key.clone(),
                etags.get(old_key).cloned().unwrap_or_default(),
            ))
        } else {
            requests.wait(requests.move_object(
                profile_id.into(),
                bucket.into(),
                old_key.clone(),
                new_key.clone(),
            ))
        };
        if let Err(error) = moved {
            return Err(JobError::Failed(
                t!(
                    "rename.stopped",
                    key = old_key.as_str(),
                    error = crate::error_details::summary(&error),
                    moved = done,
                    left = moves.len() - done
                )
                .into_owned(),
            ));
        }
        job.progress(done as u64 + 1, total);
    }
    Ok(())
}

actions!(supas3, [FocusNextRegion, FocusPreviousRegion]);

/// How the app is being left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leave {
    Quit,
    CloseWindow,
}

/// The connection of the object a file view shows, if it shows one.
fn editor_profile(view: &Entity<ObjectView>, cx: &App) -> Option<String> {
    match view.read(cx).file() {
        FileRef::Remote(object) => Some(object.profile_id.clone()),
        FileRef::Local(_) => None,
    }
}

/// What the menu and shortcuts ask of the active pane.
#[derive(Debug, Clone, Copy)]
pub enum PaneCommand {
    NewFolder,
    Rename,
    Filter,
    Copy,
    Move,
}

/// Dialog widths in rems: confirmations keep the dialog's own width; a
/// dialog with fields is a form; one with a list or a viewer is wide.
const FORM_WIDTH: f32 = 32.;
const WIDE_WIDTH: f32 = 44.;

/// One row of the sidebar.
#[derive(Clone)]
enum SidebarRow {
    Home,
    Favorite(Location),
    Recent(Location),
    Connection(Box<Profile>),
}

/// Where a connection opens: its default bucket when it has one, otherwise
/// its list of buckets.
fn start_of(profile: &Profile) -> Location {
    match &profile.compat.default_bucket {
        Some(bucket) => Location::objects(
            &profile.id,
            bucket,
            profile.compat.default_prefix.as_deref().unwrap_or(""),
        ),
        None => Location::Buckets {
            profile_id: profile.id.clone(),
        },
    }
}

impl Workspace {
    pub fn new(
        settings: Settings,
        events: std::sync::Arc<EventQueue>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connections = cx.new(|_| Connections::default());
        let mut saved = default_config_dir()
            .map(|dir| Places::load(&dir))
            .unwrap_or_default();
        let restored = saved.layout.is_some();
        let layout = saved.layout.clone().unwrap_or_default();
        let pane_layout = |pane: &PaneLayout| {
            // A folder on this computer may have gone since the last run.
            let mut pane = pane.usable(&home());
            for tab in &mut pane.tabs {
                if matches!(tab, Location::Local(path) if !path.is_dir()) {
                    *tab = home();
                }
            }
            pane
        };
        let (left_layout, right_layout) = (pane_layout(&layout.left), pane_layout(&layout.right));
        // Keep the layout as opened, so a later save writes it back intact.
        if restored {
            saved.layout = Some(Layout {
                left: left_layout.clone(),
                right: right_layout.clone(),
                left_active: layout.left_active,
            });
        }
        let places = cx.new(|_| saved);
        let left = cx.new(|cx| {
            Pane::new(
                "left",
                left_layout,
                connections.clone(),
                places.clone(),
                window,
                cx,
            )
        });
        let right = cx.new(|cx| {
            Pane::new(
                "right",
                right_layout,
                connections.clone(),
                places.clone(),
                window,
                cx,
            )
        });
        let task_center = cx.new(|cx| TaskCenter::new(events, cx));
        let details = cx.new(|_| DetailsPanel::new());
        let subscriptions = vec![
            cx.subscribe_in(&details, window, |workspace, _, event, window, cx| {
                workspace.on_details_event(event, window, cx);
            }),
            cx.subscribe_in(&left, window, |workspace, _, event, window, cx| {
                workspace.on_pane_event(Side::Left, event, window, cx);
            }),
            cx.subscribe_in(&right, window, |workspace, _, event, window, cx| {
                workspace.on_pane_event(Side::Right, event, window, cx);
            }),
            // A finished transfer may have changed what a pane shows.
            cx.subscribe(
                &task_center,
                |workspace, _, event: &TaskCenterEvent, cx| match event {
                    TaskCenterEvent::TransfersFinished => {
                        for pane in [workspace.left.clone(), workspace.right.clone()] {
                            pane.update(cx, |pane, cx| pane.location_changed(None, false, cx));
                        }
                    }
                    TaskCenterEvent::StorageFailed(errors) => {
                        workspace.storage_failed(errors, cx);
                    }
                    TaskCenterEvent::Ended(ended) => {
                        let failed = ended
                            .iter()
                            .filter_map(|(id, state)| match state {
                                TaskState::Failed(message) => Some((id.clone(), message.clone())),
                                _ => None,
                            })
                            .collect();
                        workspace.consider_retries(failed, cx);
                        let moved = ended
                            .iter()
                            .filter(|(id, _)| workspace.pending_moves.remove(id))
                            .count();
                        if moved > 0 {
                            for pane in [workspace.left.clone(), workspace.right.clone()] {
                                pane.update(cx, |pane, cx| pane.location_changed(None, false, cx));
                            }
                        }
                        for (id, state) in ended {
                            if let Some((from, to)) = workspace.pending_renames.remove(id) {
                                // Only a completed rename moves the places; a
                                // partial one leaves the old folder in place.
                                if *state == TaskState::Completed {
                                    workspace.relocate(&from, &to, cx);
                                }
                                // Whatever the outcome, objects may have moved.
                                for pane in [workspace.left.clone(), workspace.right.clone()] {
                                    pane.update(cx, |pane, cx| {
                                        pane.location_changed(None, false, cx);
                                    });
                                }
                            }
                        }
                    }
                },
            ),
            cx.observe(&connections, |_, _, cx| cx.notify()),
            // Places are small; they are written whenever they change.
            cx.observe(&places, |workspace, places, cx| {
                let saved = workspace.save_places(places.read(cx));
                if !saved && !workspace.places_unsaved {
                    workspace.places_unsaved = true;
                    workspace.notice = Some(t!("places.not_saved").into_owned());
                } else if saved && workspace.places_unsaved {
                    workspace.places_unsaved = false;
                    workspace.notice = None;
                }
                cx.notify();
            }),
            cx.observe_window_appearance(window, |workspace, window, cx| {
                if workspace.settings.appearance == Appearance::System {
                    Theme::sync_system_appearance(Some(window), cx);
                }
            }),
        ];
        let mut workspace = Self {
            connections,
            places,
            restored,
            left,
            right,
            task_center,
            details,
            sidebar_focus: cx.focus_handle(),
            sidebar_cursor: 0,
            active: if layout.left_active {
                Side::Left
            } else {
                Side::Right
            },
            settings,
            form: None,
            notice: AppBackend::start_error(cx)
                .map(|error| t!("error.backend_failed", error = error).into_owned()),
            reload_generation: 0,
            places_unsaved: false,
            pending_renames: Default::default(),
            pending_moves: Default::default(),
            retries: Default::default(),
            sidebar_squeezed: false,
            sidebar_unsqueezed: false,
            storage_warned: [false; 2],
            leaving: None,
            window: window.window_handle(),
            storage_notice: None,
            _subscriptions: subscriptions,
        };
        workspace.apply_appearance(window, cx);
        workspace.reload_connections(true, cx);
        // Closing the window from its title bar asks about unsaved edits too.
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let unsaved = this
                .read_with(cx, |workspace, cx| {
                    !workspace.unsaved_editors(cx).is_empty()
                })
                .unwrap_or(false);
            if unsaved {
                let this = this.clone();
                window.defer(cx, move |window, cx| {
                    let _ = this.update(cx, |workspace, cx| {
                        workspace.leave(Leave::CloseWindow, window, cx);
                    });
                });
            }
            !unsaved
        });
        // The keyboard starts in the active pane.
        let first = workspace.pane(workspace.active).clone();
        window.defer(cx, move |window, cx| {
            first.update(cx, |pane, cx| pane.focus(window, cx));
        });
        workspace
    }

    fn pane(&self, side: Side) -> &Entity<Pane> {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    /// Makes a pane the active one; true if it was not already.
    fn activate(&mut self, side: Side, cx: &mut Context<Self>) -> bool {
        if self.active != side {
            self.active = side;
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Reads the saved connections. On first load the right pane opens the
    /// first connection, so the window starts as "this computer | storage",
    /// unless the user has already gone somewhere in it.
    fn reload_connections(&mut self, first_load: bool, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        self.reload_generation += 1;
        let generation = self.reload_generation;
        cx.spawn(async move |workspace, cx| {
            let result = requests.profiles().await;
            let _ = workspace.update(cx, |workspace, cx| {
                // A slower, earlier read must not replace a newer one.
                if generation != workspace.reload_generation {
                    return;
                }
                match result {
                    Ok(profiles) => {
                        workspace.connections.update(cx, |connections, cx| {
                            connections.set(profiles);
                            cx.notify();
                        });
                        let first = workspace
                            .connections
                            .read(cx)
                            .profiles()
                            .first()
                            .map(start_of);
                        workspace.forget_missing_connections(cx);
                        let untouched =
                            !workspace.restored && workspace.right.read(cx).is_untouched();
                        if let (true, true, Some(location)) = (first_load, untouched, first) {
                            workspace
                                .right
                                .update(cx, |pane, cx| pane.open(location, cx));
                        }
                        // A connection's name or endpoint may have changed.
                        workspace.refresh_details(cx);
                    }
                    Err(error) => {
                        workspace.notice =
                            Some(t!("connection.load_failed", detail = error).into_owned());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Searches a location and below, and shows what was found.
    fn show_search(
        &mut self,
        side: Side,
        location: Location,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| SearchView::new(location, query.clone(), cx));
        cx.subscribe_in(&view, window, move |workspace, _, event, window, cx| {
            window.close_dialog(cx);
            match event {
                SearchViewEvent::Reveal(folder) => {
                    workspace.activate(side, cx);
                    workspace
                        .pane(side)
                        .clone()
                        .update(cx, |pane, cx| pane.open(folder.clone(), cx));
                }
                SearchViewEvent::Open(folder, entry) => {
                    workspace.show_file(RowAction::Open, folder, entry, window, cx);
                }
            }
        })
        .detach();
        let title: SharedString = t!("search.title", query = query.as_str())
            .into_owned()
            .into();
        let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title(title.clone()).width(width).child(view.clone())
        });
    }

    /// Asks for a name and creates a folder in `location`.
    fn show_new_folder(
        &mut self,
        side: Side,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let taken = self.pane(side).read(cx).loaded_names(cx);
        let initial = unique_name(&t!("folder.default_name"), true, |name| {
            taken.iter().any(|taken| taken == name)
        });
        let check = if matches!(location, Location::Local(_)) {
            check_local_name
        } else {
            check_object_name
        };
        let range = 0..initial.len();
        let dialog = cx.new(|cx| {
            NameDialog::new(
                &initial,
                range,
                None,
                check,
                taken,
                t!("folder.create").into_owned().into(),
                window,
                cx,
            )
        });
        cx.subscribe_in(&dialog, window, move |_, dialog, event, window, cx| {
            let NameDialogEvent::Submitted(name) = event;
            let Some(requests) = AppBackend::requests(cx) else {
                let message = t!("error.backend_stopped").into_owned();
                dialog.update(cx, |dialog, cx| dialog.failed(message, cx));
                return;
            };
            let (dialog, location, name) = (dialog.clone(), location.clone(), name.clone());
            cx.spawn_in(window, async move |workspace, cx| {
                let result = requests.create_folder(location.clone(), name).await;
                let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                    Ok(()) => {
                        window.close_dialog(cx);
                        workspace.pane(side).clone().update(cx, |pane, cx| {
                            pane.location_changed(Some(&location), true, cx);
                        });
                    }
                    Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                });
            })
            .detach();
        })
        .detach();
        let title: SharedString = t!("folder.new_title").into_owned().into();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .child(dialog.clone())
        });
    }

    /// Asks for a new name for one file or folder and renames it.
    fn show_rename(
        &mut self,
        side: Side,
        source: Location,
        entry: Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(source, Location::Buckets { .. }) {
            return;
        }
        let is_folder = entry.kind == EntryKind::Folder;
        let taken = self.pane(side).read(cx).loaded_names(cx);
        let check = if matches!(source, Location::Local(_)) {
            check_local_name
        } else {
            check_object_name
        };
        let range = stem_range(&entry.name, is_folder);
        let remote_folder = is_folder && matches!(source, Location::Objects { .. });
        let dialog = cx.new(|cx| {
            NameDialog::new(
                &entry.name,
                range,
                Some(entry.name.clone()),
                check,
                taken,
                t!("rename.confirm").into_owned().into(),
                window,
                cx,
            )
        });
        let title: SharedString = t!("rename.title", name = entry.name.as_str())
            .into_owned()
            .into();
        cx.subscribe_in(
            &dialog,
            window,
            move |workspace, dialog, event, window, cx| {
                let NameDialogEvent::Submitted(name) = event;
                workspace.rename(
                    side,
                    &source,
                    &entry,
                    name.clone(),
                    dialog.clone(),
                    window,
                    cx,
                );
            },
        )
        .detach();
        let note: Option<SharedString> =
            remote_folder.then(|| t!("rename.folder_note").into_owned().into());
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            let muted = cx.theme().muted_foreground;
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .when_some(note.clone(), |this, note| {
                    this.child(div().pb_2().text_xs().text_color(muted).child(note))
                })
                .child(dialog.clone())
        });
    }

    /// Carries out a rename the user confirmed.
    #[allow(clippy::too_many_arguments)]
    fn rename(
        &mut self,
        side: Side,
        source: &Location,
        entry: &Entry,
        name: String,
        dialog: Entity<NameDialog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            dialog.update(cx, |dialog, cx| dialog.failed(message, cx));
            return;
        };
        let is_folder = entry.kind == EntryKind::Folder;
        match source {
            Location::Local(folder) => {
                let (from, to) = (folder.join(&entry.name), folder.join(&name));
                let source = source.clone();
                cx.spawn_in(window, async move |workspace, cx| {
                    let result = requests.rename_local(from.clone(), to.clone()).await;
                    let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                        Ok(()) => {
                            window.close_dialog(cx);
                            if is_folder {
                                workspace.relocate(
                                    &Location::Local(from),
                                    &Location::Local(to),
                                    cx,
                                );
                            }
                            workspace.pane(side).clone().update(cx, |pane, cx| {
                                pane.location_changed(Some(&source), true, cx);
                            });
                        }
                        Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                    });
                })
                .detach();
            }
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            } if !is_folder => {
                let (profile_id, bucket) = (profile_id.clone(), bucket.clone());
                let (from, to) = (format!("{prefix}{}", entry.name), format!("{prefix}{name}"));
                let source = source.clone();
                cx.spawn_in(window, async move |workspace, cx| {
                    let result = requests.move_object(profile_id, bucket, from, to).await;
                    let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                        Ok(()) => {
                            window.close_dialog(cx);
                            workspace.pane(side).clone().update(cx, |pane, cx| {
                                pane.location_changed(Some(&source), true, cx);
                            });
                        }
                        Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                    });
                })
                .detach();
            }
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            } => {
                // A folder in storage: move every object under it, as a job.
                let old_prefix = entry
                    .prefix
                    .clone()
                    .unwrap_or_else(|| format!("{prefix}{}/", entry.name));
                let new_prefix = format!("{prefix}{name}/");
                let from = Location::objects(profile_id, bucket, &old_prefix);
                let to = Location::objects(profile_id, bucket, &new_prefix);
                let label = JobLabel {
                    kind: TaskKind::Rename,
                    name: format!("{} → {name}", entry.name),
                    source: format!("s3://{bucket}/{old_prefix}"),
                    target: format!("s3://{bucket}/{new_prefix}"),
                };
                let (profile_id, bucket) = (profile_id.clone(), bucket.clone());
                let here = source.clone();
                cx.spawn_in(window, async move |workspace, cx| {
                    // The rows shown may not include a folder of the new
                    // name further down; renaming into it would merge the
                    // two, so the bucket itself is asked.
                    let taken = requests
                        .existing(here, vec![(name.clone(), true)])
                        .await
                        .map(|found| !found.is_empty());
                    let _ = workspace.update_in(cx, |workspace, window, cx| match taken {
                        Ok(false) => {
                            let id = cx.global::<AppJobs>().start(label, 0, move |job| {
                                rename_folder(
                                    &requests,
                                    job,
                                    &profile_id,
                                    &bucket,
                                    &old_prefix,
                                    &new_prefix,
                                )
                            });
                            workspace.pending_renames.insert(id, (from, to));
                            window.close_dialog(cx);
                            Self::notify(
                                NotificationType::Info,
                                t!("rename.folder_started").into_owned(),
                                window,
                                cx,
                            );
                        }
                        Ok(true) => dialog.update(cx, |dialog, cx| {
                            dialog.failed(t!("name.exists", name = name.as_str()).into_owned(), cx);
                        }),
                        Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                    });
                })
                .detach();
            }
            // Buckets are not renamed; the dialog is not offered for them.
            Location::Buckets { .. } => window.close_dialog(cx),
        }
    }

    /// Schedules a retry of a transfer the network cut short, if it is one
    /// to retry and retries are left.
    /// Retries, after a wait, failed transfers the network cut short, and
    /// says so once for all of them.
    fn consider_retries(&mut self, failed: Vec<(String, String)>, cx: &mut Context<Self>) {
        let mut candidates = Vec::new();
        for (id, message) in failed {
            let done = self.retries.remove(&id).unwrap_or(0);
            if !crate::auto_retry::transient(&message) || done >= crate::auto_retry::MAX_ATTEMPTS {
                continue;
            }
            let Some(task) = self.task_center.read(cx).task(&id) else {
                continue;
            };
            if task.origin == crate::tasks::TaskOrigin::Transfer
                && matches!(
                    task.kind,
                    TaskKind::Upload | TaskKind::Download | TaskKind::Copy
                )
            {
                candidates.push((id, done + 1, task.name));
            }
        }
        if candidates.is_empty() {
            return;
        }
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn(async move |workspace, cx| {
            // Syncs, imports, restores and moves handle their own failures.
            let ids = candidates.iter().map(|(id, ..)| id.clone()).collect();
            let eligible = requests.auto_retry_eligible(ids).await.unwrap_or_default();
            let chosen: Vec<_> = candidates
                .into_iter()
                .filter(|(id, ..)| eligible.contains(id))
                .map(|(id, attempt, name)| {
                    // Spread at random, so transfers cut off together do
                    // not all come back at once.
                    let jitter = std::hash::BuildHasher::hash_one(
                        &std::collections::hash_map::RandomState::new(),
                        &id,
                    ) as f64
                        / u64::MAX as f64;
                    let wait = crate::auto_retry::delay(attempt, jitter);
                    (id, attempt, name, wait)
                })
                .collect();
            let _ = workspace.update(cx, |workspace, cx| {
                let message = match chosen.as_slice() {
                    [] => return,
                    [(_, attempt, name, wait)] => t!(
                        "retry.scheduled",
                        name = name.as_str(),
                        seconds = wait.as_secs(),
                        attempt = attempt,
                        of = crate::auto_retry::MAX_ATTEMPTS
                    ),
                    many => t!(
                        "retry.scheduled_many",
                        count = many.len(),
                        seconds = many
                            .iter()
                            .map(|(.., wait)| wait.as_secs())
                            .min()
                            .unwrap_or(0)
                    ),
                };
                workspace.notify_in_window(NotificationType::Info, message.into_owned(), cx);
                for (id, attempt, name, wait) in chosen {
                    workspace.retry_later(id, attempt, name, wait, cx);
                }
            });
        })
        .detach();
    }

    /// Retries one failed transfer after `wait`, unless it was removed from
    /// the list or retried by hand meanwhile.
    fn retry_later(
        &mut self,
        id: String,
        attempt: u32,
        name: String,
        wait: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(wait).await;
            let still_listed = workspace
                .read_with(cx, |workspace, cx| {
                    workspace.task_center.read(cx).task(&id).is_some()
                })
                .unwrap_or(false);
            let Some(requests) = cx.update(|cx| AppBackend::requests(cx)) else {
                return;
            };
            if !still_listed || !crate::auto_retry::claim(&id) {
                return;
            }
            let result = requests.auto_retry(id.clone()).await;
            // Queued: the old transfer stays taken, as it is gone.
            if !matches!(result, Ok(crate::backend::AutoRetry::Queued(_))) {
                crate::auto_retry::release(&id);
            }
            let _ = workspace.update(cx, |workspace, cx| match result {
                Ok(crate::backend::AutoRetry::Queued(new_id)) => {
                    workspace.retries.insert(new_id, attempt);
                    workspace
                        .task_center
                        .update(cx, |center, cx| center.forget(&id, cx));
                }
                Ok(crate::backend::AutoRetry::MaybeWritten) => {
                    let message = t!("retry.maybe_written", name = name).into_owned();
                    workspace.notify_in_window(NotificationType::Warning, message, cx);
                }
                Ok(crate::backend::AutoRetry::NotEligible) => {}
                // The check itself failed. The network again counts as this
                // attempt; anything else ends the retries, and says why.
                Err(error) if crate::auto_retry::transient(&error) => {
                    workspace.retries.insert(id.clone(), attempt);
                    workspace.consider_retries(vec![(id, error)], cx);
                }
                Err(error) => {
                    let message = t!("retry.gave_up", name = name, error = error).into_owned();
                    workspace.notify_in_window(NotificationType::Warning, message, cx);
                }
            });
        })
        .detach();
    }

    /// Shows a notification in the workspace's window, from where no
    /// window is at hand.
    fn notify_in_window(&self, kind: NotificationType, message: String, cx: &mut Context<Self>) {
        let _ = self.window.update(cx, |_, window, cx| {
            Self::notify(kind, message, window, cx);
        });
    }

    /// Says, once for each kind, that the backend could not write to disk.
    fn storage_failed(&mut self, errors: &[crate::tasks::StorageError], cx: &mut Context<Self>) {
        use crate::tasks::StorageError;
        for error in errors {
            let (kind, message) = match error {
                StorageError::Journal(detail) => (
                    0,
                    t!("storage.journal_failed", detail = detail.as_str()).into_owned(),
                ),
                StorageError::SavedJob(detail) => (
                    1,
                    t!("storage.saved_job_failed", detail = detail.as_str()).into_owned(),
                ),
            };
            if self.storage_warned[kind] {
                continue;
            }
            self.storage_warned[kind] = true;
            self.storage_notice = Some(match self.storage_notice.take() {
                Some(earlier) => format!("{earlier} {message}"),
                None => message,
            });
        }
        cx.notify();
    }

    /// Moves open tabs, favorites and recent places into a renamed folder.
    fn relocate(&mut self, from: &Location, to: &Location, cx: &mut Context<Self>) {
        for pane in [self.left.clone(), self.right.clone()] {
            pane.update(cx, |pane, cx| pane.relocate(from, to, cx));
        }
        self.places.update(cx, |places, cx| {
            places.relocate(from, to);
            cx.notify();
        });
    }

    /// Records where the panes are, and the place just opened as recent.
    fn remember_layout(&mut self, side: Side, cx: &mut Context<Self>) {
        let layout = Layout {
            left: self.left.read(cx).layout(),
            right: self.right.read(cx).layout(),
            left_active: self.active == Side::Left,
        };
        let visited = self.pane(side).read(cx).location().clone();
        self.places.update(cx, |places, cx| {
            places.visited(&visited);
            places.layout = Some(layout);
            cx.notify();
        });
    }

    /// Writes the places; false if that failed.
    fn save_places(&self, places: &Places) -> bool {
        let saved = default_config_dir()
            .map_err(|error| error.to_string())
            .and_then(|dir| places.save(&dir).map_err(|error| error.to_string()));
        if let Err(error) = &saved {
            log::warn!("could not save places: {error}");
        }
        saved.is_ok()
    }

    /// Closes tabs, favorites and recent places of connections that no
    /// longer exist, for example after the profile file was replaced.
    fn forget_missing_connections(&mut self, cx: &mut Context<Self>) {
        let known: Vec<String> = self
            .connections
            .read(cx)
            .profiles()
            .iter()
            .map(|profile| profile.id.clone())
            .collect();
        // No connections at all is more likely an unreadable profile file
        // than every connection deleted; keep the places until it is clear.
        // Deleting a connection forgets its places on its own.
        if known.is_empty() {
            return;
        }
        let mut missing: Vec<String> = Vec::new();
        {
            let places = self.places.read(cx);
            let mut consider = |location: &Location| {
                if let Some(id) = location.profile_id() {
                    if !known.iter().any(|known| known == id) && !missing.iter().any(|m| m == id) {
                        missing.push(id.to_string());
                    }
                }
            };
            places.favorites.iter().for_each(&mut consider);
            places.recent.iter().for_each(&mut consider);
            for pane in [&self.left, &self.right] {
                pane.read(cx).layout().tabs.iter().for_each(&mut consider);
            }
        }
        for id in missing {
            for pane in [self.left.clone(), self.right.clone()] {
                pane.update(cx, |pane, cx| pane.leave_profile(&id, &home(), cx));
            }
            self.places.update(cx, |places, cx| {
                places.forget_profile(&id);
                cx.notify();
            });
        }
    }

    fn other(side: Side) -> Side {
        match side {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }

    fn on_pane_event(
        &mut self,
        side: Side,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A pane whose tabs moved on their own, as after a rename elsewhere,
        // does not become the active one.
        let changed = !matches!(event, PaneEvent::Moved | PaneEvent::SelectionChanged)
            && self.activate(side, cx);
        if changed || (side == self.active && matches!(event, PaneEvent::SelectionChanged)) {
            self.refresh_details(cx);
        }
        match event {
            PaneEvent::Activated => {
                if changed {
                    self.remember_layout(side, cx);
                }
            }
            PaneEvent::Moved => self.remember_layout(side, cx),
            // Each request names the location its rows were read from, which
            // may no longer be what the pane shows.
            PaneEvent::CopyRequested(taken) => {
                let target_side = Self::other(side);
                let target = self.pane(target_side).read(cx).location().clone();
                self.start_copy(plan(&taken.source, &taken.entries, &target), window, cx);
            }
            PaneEvent::AddressesRequested { rows, public } => {
                self.copy_addresses(rows.clone(), *public, window, cx);
            }
            PaneEvent::MoveRequested(taken) => {
                let target = self.pane(Self::other(side)).read(cx).location().clone();
                let planned = plan(&taken.source, &taken.entries, &target);
                self.start_move(planned, &taken.source, window, cx);
            }
            PaneEvent::EntriesDropped(dragged, moving) => {
                let target = self.pane(side).read(cx).location().clone();
                // Dropping rows back where they came from does nothing.
                if dragged.source != target {
                    let planned = plan(&dragged.source, &dragged.entries, &target);
                    if *moving {
                        self.start_move(planned, &dragged.source, window, cx);
                    } else {
                        self.start_copy(planned, window, cx);
                    }
                }
            }
            PaneEvent::FilesDropped(paths) => {
                let target = self.pane(side).read(cx).location().clone();
                let paths = paths
                    .iter()
                    .map(|path| (path.clone(), path.is_dir()))
                    .collect();
                self.start_copy(plan_drop(paths, &target), window, cx);
            }
            PaneEvent::DeleteRequested(taken) => {
                self.confirm_delete_rows(&taken.source, &taken.entries, window, cx);
            }
            PaneEvent::SearchRequested(location, query) => {
                self.show_search(side, location.clone(), query.clone(), window, cx);
            }
            PaneEvent::Notice(message) => {
                Self::notify(NotificationType::Info, message.clone(), window, cx);
            }
            PaneEvent::NewFolderRequested(location) => {
                self.show_new_folder(side, location.clone(), window, cx);
            }
            PaneEvent::RowActionRequested(RowAction::Rename, source, entry) => {
                self.show_rename(side, source.clone(), entry.clone(), window, cx);
            }
            PaneEvent::RowActionRequested(RowAction::Sync, source, entry) => {
                self.show_sync(side, source, Some(&entry.name), window, cx);
            }
            PaneEvent::RowActionRequested(
                RowAction::DeleteBucket,
                Location::Buckets { profile_id },
                entry,
            ) => self.confirm_delete_bucket(profile_id.clone(), entry.name.clone(), window, cx),
            PaneEvent::RowActionRequested(action, source, entry) => {
                self.show_file(*action, source, entry, window, cx);
            }
            PaneEvent::SyncRequested(source) => self.show_sync(side, source, None, window, cx),
            PaneEvent::ImportRequested(location) => self.show_import(location.clone(), window, cx),
            PaneEvent::NewBucketRequested(profile_id) => {
                self.show_create_bucket(side, profile_id.clone(), window, cx)
            }
            PaneEvent::SelectionChanged => {}
        }
    }

    fn notify(
        kind: NotificationType,
        message: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.push_notification((kind, message.into()), cx);
    }

    /// A location as the user would name it, for confirmations.
    fn describe(&self, location: &Location, cx: &App) -> String {
        match location {
            Location::Local(path) => path.to_string_lossy().into_owned(),
            Location::Buckets { profile_id } => self.connections.read(cx).name_of(profile_id),
            Location::Objects {
                profile_id,
                bucket,
                prefix,
            } => format!(
                "{} / {bucket}/{prefix}",
                self.connections.read(cx).name_of(profile_id)
            ),
        }
    }

    /// Starts a planned copy, or says why it cannot be done. A copy between two connections is confirmed first,
    /// because it runs through this computer.
    fn start_copy(
        &mut self,
        planned: Result<Plan, crate::transfer_plan::Refusal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let planned = match planned {
            Ok(planned) => planned,
            Err(refusal) => {
                let message = t!(refusal.message_key()).into_owned();
                Self::notify(NotificationType::Info, message, window, cx);
                return;
            }
        };
        let Plan::CopyAcross {
            source_profile_id,
            items,
            target,
        } = &planned
        else {
            self.run_copy(planned, window, cx);
            return;
        };
        let source = match items.first() {
            Some(item) => format!(
                "{} / {}",
                self.connections.read(cx).name_of(source_profile_id),
                item.bucket
            ),
            None => String::new(),
        };
        let target_name = format!(
            "{} / {}/{}",
            self.connections.read(cx).name_of(&target.profile_id),
            target.bucket,
            target.prefix
        );
        let title: SharedString = if items.len() == 1 {
            t!("copy.across_title_one", target = target_name)
        } else {
            t!(
                "copy.across_title",
                count = items.len(),
                target = target_name
            )
        }
        .into_owned()
        .into();
        let body: SharedString = t!("copy.across_body", source = source).into_owned().into();
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (workspace, planned) = (workspace.clone(), planned.clone());
            let (enter_workspace, enter_planned) = (workspace.clone(), planned.clone());
            dialog
                .title(title.clone())
                .child(body.clone())
                // Enter does what the primary button does.
                .on_ok(move |_, window, cx| {
                    let planned = enter_planned.clone();
                    let _ = enter_workspace.update(cx, |workspace, cx| {
                        workspace.run_copy(planned, window, cx);
                    });
                    true
                })
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-copy")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-copy")
                                .primary()
                                .label(t!("action.copy").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let planned = planned.clone();
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        workspace.run_copy(planned, window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    /// Copies files on this computer as a job of the application's own.
    fn start_local_copy(&mut self, items: Vec<LocalItem>, folder: PathBuf, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = items.iter().map(|item| item.path.clone()).collect();
        let items: Vec<local_copy::CopyItem> = items
            .into_iter()
            .map(|item| local_copy::CopyItem {
                from: item.path,
                name: item.name,
                replace: match item.replace {
                    Some(Replace::File(identity)) => Some(identity),
                    _ => None,
                },
            })
            .collect();
        let name = match paths.as_slice() {
            [only] => only
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            many => t!("drag.items", count = many.len()).into_owned(),
        };
        let label = JobLabel {
            kind: TaskKind::LocalCopy,
            name,
            source: paths
                .first()
                .and_then(|path| path.parent())
                .map(|parent| parent.display().to_string())
                .unwrap_or_default(),
            target: folder.display().to_string(),
        };
        cx.global::<AppJobs>().start(label, 0, move |job| {
            let total = local_copy::total_size(&paths)
                .map_err(|error| JobError::Failed(error.to_string()))?;
            job.progress(0, total);
            local_copy::copy_into(
                &items,
                &folder,
                |done| job.progress(done, total),
                || job.cancelled(),
            )
            .map_err(|error| match error {
                local_copy::CopyError::Cancelled => JobError::Cancelled,
                local_copy::CopyError::Exists(path) => JobError::Failed(
                    t!("copy.local_exists", path = path.display().to_string()).into_owned(),
                ),
                local_copy::CopyError::Changed(path) => JobError::Failed(
                    t!("copy.replace_changed", name = path.display().to_string()).into_owned(),
                ),
                other => JobError::Failed(other.to_string()),
            })
        });
    }

    fn run_copy(&mut self, planned: Plan, window: &mut Window, cx: &mut Context<Self>) {
        self.check_targets(planned, None, window, cx);
    }

    /// Checks the target for names the copy or move would use, asks what to
    /// do about any it finds, then carries it out. `moving` names the source
    /// of a move.
    fn check_targets(
        &mut self,
        planned: Plan,
        moving: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            Self::notify(NotificationType::Error, message, window, cx);
            return;
        };
        let (target, names) = (planned.target(), planned.target_names());
        cx.spawn_in(window, async move |workspace, cx| {
            let profile_id = target.profile_id().map(str::to_string);
            let found = requests.existing(target, names).await;
            // Whether the service itself refuses a replacement of a changed
            // object, read from the client the backend builds.
            let atomic = match (&found, profile_id) {
                (Ok(found), Some(profile_id)) if !found.is_empty() => requests
                    .write_traits(profile_id)
                    .await
                    .is_ok_and(|traits| traits.refuses_changed_saves),
                _ => false,
            };
            let _ = workspace.update_in(cx, |workspace, window, cx| match found {
                Err(error) => {
                    let message = t!("copy.check_failed", detail = error).into_owned();
                    Self::notify(NotificationType::Error, message, window, cx);
                }
                Ok(found) if conflicts(&planned, &found).is_empty() => match moving {
                    Some(source) => workspace.run_move_now(planned, source, window, cx),
                    None => workspace.run_copy_now(planned, window, cx),
                },
                Ok(found) => {
                    workspace.ask_about_conflicts(planned, found, atomic, moving, window, cx);
                }
            });
        })
        .detach();
    }

    /// Asks whether to skip, replace or keep both for names already taken.
    /// A move offers no replacing: it would delete two things at once.
    fn ask_about_conflicts(
        &mut self,
        planned: Plan,
        found: Vec<Existing>,
        atomic: bool,
        moving: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let taken: Vec<String> = conflicts(&planned, &found)
            .iter()
            .map(|found| found.name.clone())
            .collect();
        let title: SharedString = match taken.as_slice() {
            [only] => t!("conflict.title_one", name = only.as_str())
                .into_owned()
                .into(),
            many => t!("conflict.title_many", count = many.len())
                .into_owned()
                .into(),
        };
        let listed: SharedString = {
            let mut shown: Vec<String> = taken.iter().take(8).cloned().collect();
            if taken.len() > 8 {
                shown.push(t!("conflict.more", count = taken.len() - 8).into_owned());
            }
            shown.join("\n").into()
        };
        let note: SharedString = if atomic {
            t!("conflict.replace_note_atomic").into_owned().into()
        } else {
            t!("conflict.replace_note").into_owned().into()
        };
        let workspace = cx.weak_entity();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let choose = |choice: Choice| {
                let (workspace, planned, found) =
                    (workspace.clone(), planned.clone(), found.clone());
                let moving = moving.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    window.close_dialog(cx);
                    let (planned, found, moving) = (planned.clone(), found.clone(), moving.clone());
                    let _ = workspace.update(cx, |workspace, cx| {
                        workspace.apply_choice(planned, found, choice, moving, window, cx);
                    });
                }
            };
            dialog
                .title(title.clone())
                .width(width)
                // The choice matters; Enter alone does not make it.
                .on_ok(|_, _, _| false)
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().child(t!("conflict.body").into_owned()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .whitespace_normal()
                                .child(listed.clone()),
                        )
                        .when(moving.is_none(), |this| {
                            this.child(div().text_xs().text_color(muted).child(note.clone()))
                        }),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("conflict-cancel")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .when(moving.is_none(), |this| {
                            this.child(
                                Button::new("conflict-replace")
                                    .danger()
                                    .label(t!("conflict.replace").into_owned())
                                    .on_click(choose(Choice::Replace)),
                            )
                        })
                        .child(
                            Button::new("conflict-both")
                                .outline()
                                .label(t!("conflict.keep_both").into_owned())
                                .on_click(choose(Choice::KeepBoth)),
                        )
                        .child(
                            Button::new("conflict-skip")
                                .primary()
                                .label(t!("conflict.skip").into_owned())
                                .on_click(choose(Choice::Skip)),
                        ),
                )
        });
    }

    /// Carries out the user's choice about names already taken. New names
    /// for "keep both" are checked at the target too, and chosen again if
    /// one of them is taken by something not seen before.
    fn apply_choice(
        &mut self,
        planned: Plan,
        mut found: Vec<Existing>,
        choice: Choice,
        moving: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            Self::notify(NotificationType::Error, message, window, cx);
            return;
        };
        cx.spawn_in(window, async move |workspace, cx| {
            let (mut plan, mut left_out) = resolve(planned.clone(), &found, choice);
            let mut settled = choice != Choice::KeepBoth;
            for _ in 0..5 {
                if settled {
                    break;
                }
                match requests.existing(plan.target(), plan.target_names()).await {
                    Ok(more) if conflicts(&plan, &more).is_empty() => settled = true,
                    Ok(more) => {
                        found.extend(more);
                        (plan, left_out) = resolve(planned.clone(), &found, choice);
                    }
                    Err(error) => {
                        let _ = workspace.update_in(cx, |_, window, cx| {
                            let message = t!("copy.check_failed", detail = error).into_owned();
                            Self::notify(NotificationType::Error, message, window, cx);
                        });
                        return;
                    }
                }
            }
            let _ = workspace.update_in(cx, |workspace, window, cx| {
                if !settled {
                    let message = t!("conflict.names_taken").into_owned();
                    Self::notify(NotificationType::Error, message, window, cx);
                    return;
                }
                if !left_out.is_empty() {
                    let message = t!("conflict.left_out", names = left_out.join(", ")).into_owned();
                    Self::notify(NotificationType::Info, message, window, cx);
                }
                if plan.is_empty() {
                    return;
                }
                match moving {
                    Some(source) => workspace.run_move_now(plan, source, window, cx),
                    None => workspace.run_copy_now(plan, window, cx),
                }
            });
        })
        .detach();
    }

    /// Files open in tabs that hold unsaved edits or a save in flight.
    fn unsaved_editors(&self, cx: &App) -> Vec<(String, Entity<ObjectView>)> {
        [&self.left, &self.right]
            .into_iter()
            .flat_map(|pane| pane.read(cx).editors())
            .filter(|(_, view)| view.read(cx).holds_work(cx))
            .collect()
    }

    /// Quits or closes the window, asking first what to do with unsaved
    /// edits: save them all, discard them, or stay.
    pub fn leave(&mut self, how: Leave, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self.unsaved_editors(cx);
        if unsaved.is_empty() {
            Self::leave_now(how, window, cx);
            return;
        }
        let names: SharedString = unsaved
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .into();
        // Saving all is offered only when every one of them can be saved.
        let savable = unsaved.iter().all(|(_, view)| {
            let view = view.read(cx);
            view.is_editable() && !view.is_saving()
        });
        let title: SharedString = t!("leave.title", count = unsaved.len()).into_owned().into();
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let (save_workspace, discard_workspace) = (workspace.clone(), workspace.clone());
            let views: Vec<Entity<ObjectView>> =
                unsaved.iter().map(|(_, view)| view.clone()).collect();
            dialog
                .title(title.clone())
                .on_ok(|_, _, _| false)
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().child(t!("leave.body").into_owned()))
                        .child(div().text_sm().text_color(muted).child(names.clone())),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("leave-cancel")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("leave-discard")
                                .danger()
                                .label(t!("view.discard").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = discard_workspace.update(cx, |_, cx| {
                                        Self::leave_now(how, window, cx);
                                    });
                                }),
                        )
                        .when(savable, |this| {
                            this.child(
                                Button::new("leave-save")
                                    .primary()
                                    .label(t!("leave.save_all").into_owned())
                                    .on_click(move |_, window, cx| {
                                        window.close_dialog(cx);
                                        let views = views.clone();
                                        let _ = save_workspace.update(cx, |workspace, cx| {
                                            workspace.save_all_then(how, views, cx);
                                        });
                                    }),
                            )
                        }),
                )
        });
    }

    fn leave_now(how: Leave, window: &mut Window, cx: &mut App) {
        match how {
            Leave::Quit => crate::quit(cx),
            Leave::CloseWindow => window.remove_window(),
        }
    }

    /// Saves every view, then leaves once all have been saved.
    fn save_all_then(
        &mut self,
        how: Leave,
        views: Vec<Entity<ObjectView>>,
        cx: &mut Context<Self>,
    ) {
        let pending: std::collections::HashSet<EntityId> = views
            .iter()
            .filter(|view| view.read(cx).is_dirty(cx))
            .map(|view| view.entity_id())
            .collect();
        self.leaving = Some((how, pending));
        for view in views {
            view.update(cx, |view, cx| view.save(cx));
        }
        // Nothing was left to save.
        self.saved_while_leaving(EntityId::from(0u64), cx);
    }

    /// One save finished; leave when it was the last one waited for.
    fn saved_while_leaving(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let Some((how, pending)) = &mut self.leaving else {
            return;
        };
        pending.remove(&id);
        if !pending.is_empty() {
            return;
        }
        let how = *how;
        self.leaving = None;
        // Asked again rather than left at once: something may have been
        // edited while the saves ran.
        let handle = self.window;
        let this = cx.entity();
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                this.update(cx, |workspace, cx| workspace.leave(how, window, cx));
            });
        });
    }

    /// A save failed while leaving: stay, with the edits shown.
    fn save_failed_while_leaving(&mut self, cx: &mut Context<Self>) {
        if self.leaving.take().is_none() {
            return;
        }
        let handle = self.window;
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| {
                let message = t!("leave.save_failed").into_owned();
                Self::notify(NotificationType::Error, message, window, cx);
            });
        });
    }

    /// Confirms a move, which removes each source once its copy is checked.
    fn start_move(
        &mut self,
        planned: Result<Plan, crate::transfer_plan::Refusal>,
        source: &Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let planned = match planned {
            Ok(planned) => planned,
            Err(refusal) => {
                let message = t!(refusal.message_key()).into_owned();
                Self::notify(NotificationType::Info, message, window, cx);
                return;
            }
        };
        let source = self.describe(source, cx);
        let target = self.describe(&planned.target(), cx);
        let names = planned.target_names();
        let title: SharedString = match names.as_slice() {
            [(only, _)] => t!("move.title_one", name = only.as_str()),
            many => t!("move.title_many", count = many.len()),
        }
        .into_owned()
        .into();
        let mut body = t!("move.body").into_owned();
        if matches!(planned, Plan::CopyAcross { .. }) {
            body.push(' ');
            body.push_str(&t!("move.body_across"));
        }
        let body: SharedString = body.into();
        let destination: SharedString = t!("move.to", target = target).into_owned().into();
        let workspace = cx.weak_entity();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let (workspace, planned, source) = (workspace.clone(), planned.clone(), source.clone());
            dialog
                .title(title.clone())
                .width(width)
                // Moving removes the sources; Enter alone does not start it.
                .on_ok(|_, _, _| false)
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .child(destination.clone()),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .whitespace_normal()
                                .child(body.clone()),
                        ),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-move")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-move")
                                .primary()
                                .label(t!("action.move").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let (planned, source) = (planned.clone(), source.clone());
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        workspace.check_targets(planned, Some(source), window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    /// Runs a move as a job of the application's own; the panes are
    /// reloaded when it ends.
    fn run_move_now(
        &mut self,
        planned: Plan,
        source: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            Self::notify(NotificationType::Error, message, window, cx);
            return;
        };
        let names = planned.target_names();
        let label = JobLabel {
            kind: TaskKind::Move,
            name: match names.as_slice() {
                [(only, _)] => only.clone(),
                many => t!("drag.items", count = many.len()).into_owned(),
            },
            source,
            target: self.describe(&planned.target(), cx),
        };
        let id = cx.global::<AppJobs>().start(label, 0, move |job| {
            match crate::moves::run(&planned, &requests, job) {
                Ok(report) if report.kept.is_empty() => Ok(()),
                Ok(report) => Err(JobError::Failed(crate::moves::describe(&report))),
                Err(crate::moves::Stopped::Cancelled(_)) => Err(JobError::Cancelled),
            }
        });
        self.pending_moves.insert(id);
        Self::notify(
            NotificationType::Info,
            t!("move.started").into_owned(),
            window,
            cx,
        );
    }

    fn run_copy_now(&mut self, planned: Plan, window: &mut Window, cx: &mut Context<Self>) {
        if let Plan::CopyLocal { items, folder } = planned {
            self.start_local_copy(items, folder, cx);
            return;
        }
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            Self::notify(NotificationType::Error, message, window, cx);
            return;
        };
        // A copy within one connection is done by the service before the
        // request returns, object by object. It can fail part-way, so the
        // target is reloaded whatever the outcome.
        let direct = matches!(planned, Plan::CopyWithin { .. });
        cx.spawn_in(window, async move |workspace, cx| {
            let result = requests.execute(planned).await;
            let _ = workspace.update_in(cx, |workspace, window, cx| {
                if direct {
                    for pane in [workspace.left.clone(), workspace.right.clone()] {
                        pane.update(cx, |pane, cx| pane.location_changed(None, false, cx));
                    }
                }
                match result {
                    Ok(outcome) => {
                        if outcome.copied > 0 {
                            let message = t!("copy.copied", count = outcome.copied).into_owned();
                            Self::notify(NotificationType::Success, message, window, cx);
                        }
                        if !outcome.failures.is_empty() {
                            let mut message = t!(
                                "copy.some_failed",
                                count = outcome.failures.len(),
                                detail = outcome.failures.join("; ")
                            )
                            .into_owned();
                            if direct {
                                message.push(' ');
                                message.push_str(&t!("copy.maybe_partial"));
                            }
                            Self::notify(NotificationType::Error, message, window, cx);
                        }
                    }
                    Err(error) => {
                        let message = t!("copy.failed", detail = error).into_owned();
                        Self::notify(NotificationType::Error, message, window, cx);
                    }
                }
            });
        })
        .detach();
    }

    fn confirm_delete_rows(
        &mut self,
        location: &Location,
        rows: &[Entry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let location = location.clone();
        if let Some(deletion) = plan_local_deletion(&location, rows) {
            self.confirm_local_deletion(deletion, location, window, cx);
            return;
        }
        let deletion = match plan_deletion(&location, rows) {
            Ok(deletion) => deletion,
            Err(refusal) => {
                let message = t!(refusal.message_key()).into_owned();
                Self::notify(NotificationType::Info, message, window, cx);
                return;
            }
        };
        let title: SharedString = match deletion.names.as_slice() {
            [only] => t!("delete.title_one", name = only).into_owned().into(),
            many => t!("delete.title_many", count = many.len())
                .into_owned()
                .into(),
        };
        let body: SharedString = t!("delete.body", location = self.describe(&location, cx))
            .into_owned()
            .into();
        let workspace = cx.weak_entity();
        let changed = location.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let (workspace, deletion) = (workspace.clone(), deletion.clone());
            let changed = changed.clone();
            dialog
                .title(title.clone())
                .child(body.clone())
                // Deleting cannot be undone: Enter alone must not do it. The
                // dialog stays open until Delete or Cancel is chosen.
                .on_ok(|_, _, _| false)
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-delete-rows")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-delete-rows")
                                .danger()
                                .label(t!("action.delete").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let (deletion, changed) = (deletion.clone(), changed.clone());
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        workspace.run_delete(deletion, changed, window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    /// Asks before moving files on this computer to the recycle bin.
    fn confirm_local_deletion(
        &mut self,
        deletion: LocalDeletion,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title: SharedString = match deletion.names.as_slice() {
            [only] => t!("delete.trash_title_one", name = only)
                .into_owned()
                .into(),
            many => t!("delete.trash_title_many", count = many.len())
                .into_owned()
                .into(),
        };
        let body: SharedString = t!("delete.local_body").into_owned().into();
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (workspace, deletion, location) =
                (workspace.clone(), deletion.clone(), location.clone());
            dialog
                .title(title.clone())
                .child(body.clone())
                .on_ok(|_, _, _| false)
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-trash")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-trash")
                                .danger()
                                .label(t!("delete.move_to_trash").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let (paths, location) =
                                        (deletion.paths.clone(), location.clone());
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        workspace.run_trash(paths, location, window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn run_trash(
        &mut self,
        paths: Vec<PathBuf>,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn_in(window, async move |workspace, cx| {
            let result = requests.trash_local(paths).await;
            let _ = workspace.update_in(cx, |workspace, window, cx| {
                for pane in [workspace.left.clone(), workspace.right.clone()] {
                    pane.update(cx, |pane, cx| {
                        pane.location_changed(Some(&location), true, cx)
                    });
                }
                match result {
                    Ok(count) => Self::notify(
                        NotificationType::Success,
                        t!("delete.trashed", count = count).into_owned(),
                        window,
                        cx,
                    ),
                    Err(error) => Self::notify(
                        NotificationType::Error,
                        t!("delete.trash_failed", detail = error).into_owned(),
                        window,
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    fn run_delete(
        &mut self,
        deletion: Deletion,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            Self::notify(NotificationType::Error, message, window, cx);
            return;
        };
        cx.spawn_in(window, async move |workspace, cx| {
            let result = requests.delete(deletion).await;
            let _ = workspace.update_in(cx, |workspace, window, cx| {
                // Reload either way, at once: a failure may have deleted part
                // of it, and the rows and marks on screen are now out of date.
                for pane in [workspace.left.clone(), workspace.right.clone()] {
                    pane.update(cx, |pane, cx| {
                        pane.location_changed(Some(&location), true, cx)
                    });
                }
                match result {
                    Ok(count) => {
                        let message = t!("delete.done", count = count).into_owned();
                        Self::notify(NotificationType::Success, message, window, cx);
                    }
                    Err(error) => {
                        let message = t!("delete.failed", detail = error).into_owned();
                        Self::notify(NotificationType::Error, message, window, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Opens the dialog for one file: its content, its links or its history.
    fn show_file(
        &mut self,
        action: RowAction,
        source: &Location,
        entry: &Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action == RowAction::Properties {
            self.show_details_of(source, entry, window, cx);
            return;
        }
        let Some(file) = file_of(source, entry) else {
            return;
        };
        let name: SharedString = entry.name.clone().into();
        let Some(object) = file.remote().cloned() else {
            // A local file: nothing to ask the backend first.
            if action == RowAction::Open {
                self.open_object_view(file, entry, source, true, window, cx);
            }
            return;
        };
        if action == RowAction::Versions {
            let view = cx.new(|cx| VersionsView::new(object, cx));
            let title: SharedString = t!("versions.title", name = name).into_owned().into();
            let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
            window.open_dialog(cx, move |dialog, _, _| {
                dialog.title(title.clone()).width(width).child(view.clone())
            });
            return;
        }
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            Self::notify(NotificationType::Error, message, window, cx);
            return;
        };
        let profile = self.connections.read(cx).get(&object.profile_id).cloned();
        let (entry, source) = (entry.clone(), source.clone());
        // What is said about saving and addresses depends on the client the
        // backend actually builds, which may have taken an endpoint from the
        // environment. Without an answer, assume the less convenient case.
        cx.spawn_in(window, async move |workspace, cx| {
            let traits = requests.write_traits(object.profile_id.clone()).await.ok();
            let _ = workspace.update_in(cx, |workspace, window, cx| match action {
                RowAction::Open => {
                    let guarded = traits.is_some_and(|traits| traits.refuses_changed_saves);
                    workspace.open_object_view(file, &entry, &source, guarded, window, cx);
                }
                RowAction::Share => {
                    let early = profile.as_ref().is_none_or(link_may_expire_early);
                    let plain = match (&profile, traits) {
                        (Some(profile), Some(traits)) => {
                            plain_address_is_known(profile, traits.custom_endpoint)
                        }
                        _ => false,
                    };
                    let view = cx.new(|cx| ShareView::new(object, early, plain, cx));
                    let title: SharedString = t!("share.title", name = name).into_owned().into();
                    let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
                    window.open_dialog(cx, move |dialog, _, _| {
                        dialog.title(title.clone()).width(width).child(view.clone())
                    });
                }
                _ => {}
            });
        })
        .detach();
    }

    fn open_object_view(
        &mut self,
        file: FileRef,
        entry: &Entry,
        source: &Location,
        guarded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name: SharedString = entry.name.clone().into();
        let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
        // Text opens in a tab of the pane it was opened from; images and
        // notes in a dialog.
        let in_tab = matches!(
            crate::object_actions::opening(&entry.name, entry.size),
            crate::object_actions::Opening::Text
        );
        let view = cx
            .new(|cx| ObjectView::new(file, &entry.name, entry.size, guarded, in_tab, window, cx));
        let changed = source.clone();
        cx.subscribe(&view, move |workspace, view, event, cx| match event {
            ObjectViewEvent::Saved => {
                for pane in [workspace.left.clone(), workspace.right.clone()] {
                    pane.update(cx, |pane, cx| {
                        pane.location_changed(Some(&changed), false, cx)
                    });
                }
                workspace.saved_while_leaving(view.entity_id(), cx);
            }
            ObjectViewEvent::SaveFailed => workspace.save_failed_while_leaving(cx),
            ObjectViewEvent::CloseRequested => {}
        })
        .detach();
        if in_tab {
            let side = if self.left.read(cx).location() == source {
                Side::Left
            } else if self.right.read(cx).location() == source {
                Side::Right
            } else {
                self.active
            };
            self.activate(side, cx);
            self.pane(side).clone().update(cx, |pane, cx| {
                pane.open_editor(entry.name.clone(), view, window, cx);
            });
            return;
        }
        window.open_dialog(cx, move |dialog, _, cx| {
            // Unsaved edits and a save in flight are not lost to a stray
            // Escape, Enter or click; the view's own Close button asks first.
            let held = view.read(cx).holds_work(cx);
            dialog
                .title(name.clone())
                .width(width)
                .keyboard(!held)
                .overlay_closable(!held)
                .close_button(!held)
                .on_ok(|_, _, _| false)
                .child(view.clone())
        });
    }

    /// Opens the dialog that syncs a local folder to what the other pane
    /// shows, or says why that cannot be done.
    fn show_sync(
        &mut self,
        side: Side,
        source: &Location,
        folder: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.pane(Self::other(side)).read(cx).location().clone();
        let pair = match sync_pair(source, folder, &target) {
            Ok(pair) => pair,
            Err(refusal) => {
                let message = t!(refusal.message_key()).into_owned();
                Self::notify(NotificationType::Info, message, window, cx);
                return;
            }
        };
        let view = cx.new(|cx| SyncView::new(pair, window, cx));
        let title: SharedString = t!("sync.title").into_owned().into();
        let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, cx| {
            let enter = view.clone();
            // Once a sync is being started it goes ahead; the dialog stays
            // until the answer arrives, so what it shows stays true.
            let starting = view.read(cx).is_starting();
            dialog
                .keyboard(!starting)
                .overlay_closable(!starting)
                .close_button(!starting)
                .title(title.clone())
                .width(width)
                // Enter starts the sync when it may start, and never closes
                // the comparison by itself.
                .on_ok(move |_, window, cx| {
                    enter.update(cx, |view, cx| view.start(window, cx));
                    false
                })
                .child(view.clone())
        });
    }

    /// Says what this application is, which version, and under what terms.
    /// The connection behind a location, as the details panel shows it.
    fn connection_info(&self, location: &Location, cx: &App) -> Option<ConnectionInfo> {
        let profile = self
            .connections
            .read(cx)
            .get(location.profile_id()?)?
            .clone();
        let endpoint = match &profile.credential_type {
            supas3_core::credentials::CredentialType::CustomEndpoint { endpoint_url, .. } => {
                Some(endpoint_url.clone())
            }
            _ => None,
        };
        Some(ConnectionInfo {
            idrive_e2: profile.compat.provider == supas3_core::credentials::Provider::IdriveE2,
            name: profile.name,
            endpoint,
            region: profile.region,
        })
    }

    /// Shows what the active pane has selected in the details panel, when
    /// the panel is open.
    fn refresh_details(&mut self, cx: &mut Context<Self>) {
        if !self.settings.details_open {
            return;
        }
        let (source, subject) = self.pane(self.active).read(cx).details_subject(cx);
        let connection = self.connection_info(&source, cx);
        self.details.update(cx, |details, cx| {
            details.show(source, subject, connection, cx);
        });
    }

    /// Opens the details panel on one row.
    fn show_details_of(
        &mut self,
        source: &Location,
        entry: &Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.details_open {
            self.store_settings(|settings| settings.details_open = true, window, cx);
        }
        let subject = crate::details::subject(source, std::slice::from_ref(entry), 0);
        let connection = self.connection_info(source, cx);
        let source = source.clone();
        self.details.update(cx, |details, cx| {
            details.show(source, subject, connection, cx);
        });
    }

    /// Shows or hides the details panel.
    pub fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.store_settings(
            |settings| settings.details_open = !settings.details_open,
            window,
            cx,
        );
        self.refresh_details(cx);
    }

    fn on_details_event(
        &mut self,
        event: &DetailsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DetailsEvent::Close => {
                self.store_settings(|settings| settings.details_open = false, window, cx);
            }
            DetailsEvent::Row(action, source, entry) => {
                self.show_file(*action, source, entry, window, cx);
            }
            DetailsEvent::Download(source, entry) => {
                self.download(source.clone(), entry.clone(), window, cx);
            }
            DetailsEvent::FolderAccess {
                profile_id,
                bucket,
                prefix,
                to,
            } => {
                let (profile_id, bucket, prefix) =
                    (profile_id.clone(), bucket.clone(), prefix.clone());
                self.confirm_folder_access(profile_id, bucket, prefix, *to, window, cx);
            }
            DetailsEvent::SetVersioning {
                profile_id,
                bucket,
                enabled,
            } => {
                let (profile_id, bucket) = (profile_id.clone(), bucket.clone());
                self.confirm_versioning(profile_id, bucket, *enabled, window, cx);
            }
            DetailsEvent::EditPolicy { profile_id, bucket } => {
                self.show_policy(profile_id.clone(), bucket.clone(), window, cx);
            }
            DetailsEvent::EditMetadata { object, seen } => {
                self.show_metadata(object.clone(), seen.clone(), window, cx);
            }
            DetailsEvent::CopyAddress(source, entry) => {
                let rows = DraggedEntries {
                    source: source.clone(),
                    entries: std::rc::Rc::new(vec![entry.clone()]),
                };
                self.copy_addresses(rows, false, window, cx);
            }
            DetailsEvent::ChangeContentType {
                object,
                name,
                current,
                etag,
            } => {
                let (object, etag) = (object.clone(), etag.clone());
                self.show_content_type(object, name, current.clone(), etag, window, cx);
            }
            DetailsEvent::ChangeAccess {
                object,
                to,
                current,
                etag,
            } => {
                let (object, etag) = (object.clone(), etag.clone());
                self.confirm_access(object, *to, *current, etag, window, cx);
            }
        }
    }

    /// Puts the rows' addresses on the clipboard, one a line: where they
    /// are, or for files in storage their plain web addresses.
    fn copy_addresses(
        &mut self,
        rows: DraggedEntries,
        public: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let done = |lines: Vec<String>, window: &mut Window, cx: &mut App| {
            let count = lines.len();
            cx.write_to_clipboard(ClipboardItem::new_string(lines.join("\n")));
            let message = t!("address.copied", count = count).into_owned();
            Self::notify(NotificationType::Success, message, window, cx);
        };
        if !public {
            let lines: Vec<String> = rows
                .entries
                .iter()
                .filter_map(|entry| address_of(&rows.source, entry))
                .collect();
            if !lines.is_empty() {
                done(lines, window, cx);
            }
            return;
        }
        let Location::Objects {
            profile_id,
            bucket,
            prefix,
        } = rows.source.clone()
        else {
            return;
        };
        let keys: Vec<String> = rows
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::File)
            .map(|entry| format!("{prefix}{}", entry.name))
            .collect();
        if keys.is_empty() {
            let message = t!("address.files_only").into_owned();
            Self::notify(NotificationType::Info, message, window, cx);
            return;
        }
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        let profile = self.connections.read(cx).get(&profile_id).cloned();
        cx.spawn_in(window, async move |_, cx| {
            // As in the share dialog: only where the plain address is known,
            // not guessed for an endpoint taken from the environment.
            let traits = requests.write_traits(profile_id.clone()).await.ok();
            let known = match (&profile, traits) {
                (Some(profile), Some(traits)) => {
                    plain_address_is_known(profile, traits.custom_endpoint)
                }
                _ => false,
            };
            let result = if known {
                requests.plain_links(profile_id, bucket, keys).await
            } else {
                Err(t!("address.unknown").into_owned())
            };
            let _ = cx.update(|window, cx| match result {
                Ok(lines) => done(lines, window, cx),
                Err(error) => {
                    let message = t!("address.failed", detail = brief_error(&error)).into_owned();
                    Self::notify(NotificationType::Error, message, window, cx);
                }
            });
        })
        .detach();
    }

    /// An object was changed from the details panel: shows it anew, and
    /// the listings of its folder.
    fn object_changed(&mut self, object: &ObjectRef, cx: &mut Context<Self>) {
        self.details.update(cx, |details, cx| details.reload(cx));
        let prefix = object.key.rfind('/').map_or("", |end| &object.key[..=end]);
        let folder = Location::objects(&object.profile_id, &object.bucket, prefix);
        for pane in [self.left.clone(), self.right.clone()] {
            pane.update(cx, |pane, cx| {
                pane.location_changed(Some(&folder), false, cx)
            });
        }
    }

    /// Asks for a new Content-Type and changes it, only on the content
    /// the panel showed.
    fn show_content_type(
        &mut self,
        object: ObjectRef,
        name: &str,
        current: Option<String>,
        etag: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| ContentTypeDialog::new(name, current, window, cx));
        cx.subscribe_in(&dialog, window, move |_, dialog, event, window, cx| {
            let ContentTypeEvent::Submitted(content_type) = event;
            let Some(requests) = AppBackend::requests(cx) else {
                let message = t!("error.backend_stopped").into_owned();
                dialog.update(cx, |dialog, cx| dialog.failed(message, cx));
                return;
            };
            let (dialog, object, content_type, etag) = (
                dialog.clone(),
                object.clone(),
                content_type.clone(),
                etag.clone(),
            );
            cx.spawn_in(window, async move |workspace, cx| {
                let result = requests
                    .set_content_type(object.clone(), content_type, etag)
                    .await
                    .map_err(|error| {
                        // The two refusals made before anything is written.
                        if error.contains("changed since it was shown") {
                            t!("content_type.changed").into_owned()
                        } else if error.contains("only for objects up to 5 GiB") {
                            t!("properties.too_large").into_owned()
                        } else {
                            error
                        }
                    });
                let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                    Ok(()) => {
                        window.close_dialog(cx);
                        workspace.object_changed(&object, cx);
                    }
                    Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                });
            })
            .detach();
        })
        .detach();
        let title: SharedString = t!("content_type.title", name = name).into_owned().into();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .child(dialog.clone())
        });
    }

    /// Lists the objects under a folder, then asks whether to give each the
    /// ACL `to`, saying how many there are and what it means.
    fn confirm_folder_access(
        &mut self,
        profile_id: String,
        bucket: String,
        prefix: String,
        to: Access,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        let Some(identity) = self
            .connections
            .read(cx)
            .get(&profile_id)
            .map(|profile| profile.cache_identity())
        else {
            return;
        };
        let on_oss = self
            .connections
            .read(cx)
            .get(&profile_id)
            .is_some_and(|profile| Service::of(profile) == Service::Oss);
        cx.spawn_in(window, async move |workspace, cx| {
            let mut listed = requests
                .list_keys(profile_id.clone(), bucket.clone(), prefix.clone())
                .await;
            // The first object tells whether the service lets ACLs be read
            // and changed here at all.
            if let Some(first) = listed.as_ref().ok().and_then(|keys| keys.first()) {
                let object = ObjectRef {
                    profile_id: profile_id.clone(),
                    bucket: bucket.clone(),
                    key: first.key.clone(),
                };
                match requests.object_permissions(object).await {
                    Ok(permissions) if permissions.status == "available" => {}
                    Ok(permissions) => {
                        let key = match permissions.status.as_str() {
                            "unsupported" => "access.unsupported",
                            "access_denied" => "access.denied",
                            _ => "access.unknown",
                        };
                        listed = Err(t!(key).into_owned());
                    }
                    Err(error) => listed = Err(error),
                }
            }
            let _ = workspace.update_in(cx, |_, window, cx| {
                let keys: Vec<String> = match listed {
                    Ok(keys) => keys.into_iter().map(|listed| listed.key).collect(),
                    Err(error) => {
                        let message =
                            t!("access.failed", detail = brief_error(&error)).into_owned();
                        Self::notify(NotificationType::Error, message, window, cx);
                        return;
                    }
                };
                if keys.is_empty() || keys.len() > FOLDER_ACCESS_LIMIT {
                    let message = if keys.is_empty() {
                        t!("folder_access.empty").into_owned()
                    } else {
                        t!(
                            "folder_access.too_many",
                            count = keys.len(),
                            limit = FOLDER_ACCESS_LIMIT
                        )
                        .into_owned()
                    };
                    Self::notify(NotificationType::Info, message, window, cx);
                    return;
                }
                let address = format!("s3://{bucket}/{prefix}");
                let mut lines = vec![
                    t!("folder_access.count", count = keys.len()).into_owned(),
                    t!(if to == Access::PublicRead {
                        "access.public_body"
                    } else {
                        "access.private_body"
                    })
                    .into_owned(),
                    t!("folder_access.replaced").into_owned(),
                ];
                if on_oss {
                    lines.push(t!("access.inherit_note").into_owned());
                }
                lines.dedup();
                let (title, confirm): (SharedString, SharedString) = match to {
                    Access::PublicRead => (
                        t!("folder_access.public_title").into_owned().into(),
                        t!("access.make_public").into_owned().into(),
                    ),
                    _ => (
                        t!("folder_access.private_title").into_owned().into(),
                        t!("access.make_private").into_owned().into(),
                    ),
                };
                let this = cx.weak_entity();
                window.open_dialog(cx, move |dialog, _, cx| {
                    let muted = cx.theme().muted_foreground;
                    let (this, keys, identity) = (this.clone(), keys.clone(), identity.clone());
                    let (profile_id, bucket, prefix) =
                        (profile_id.clone(), bucket.clone(), prefix.clone());
                    dialog
                        .title(title.clone())
                        // Enter does not change who may read data.
                        .on_ok(|_, _, _| false)
                        .child(
                            v_flex()
                                .gap_2()
                                .child(div().text_xs().text_color(muted).child(address.clone()))
                                .children(
                                    lines.iter().map(|line| div().text_sm().child(line.clone())),
                                ),
                        )
                        .footer(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("cancel-folder-access")
                                        .outline()
                                        .label(t!("action.cancel").into_owned())
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("confirm-folder-access")
                                        .when(to == Access::PublicRead, |this| this.danger())
                                        .label(confirm.clone())
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            let job = (
                                                profile_id.clone(),
                                                bucket.clone(),
                                                prefix.clone(),
                                                keys.clone(),
                                                identity.clone(),
                                            );
                                            let _ = this.update(cx, |workspace, cx| {
                                                workspace.start_folder_access(job, to, window, cx);
                                            });
                                        }),
                                ),
                        )
                });
            });
        })
        .detach();
    }

    /// Gives each listed object the ACL `to`, as a job of the task center.
    fn start_folder_access(
        &mut self,
        (profile_id, bucket, prefix, keys, identity): (String, String, String, Vec<String>, String),
        to: Access,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        let label = JobLabel {
            kind: TaskKind::Access,
            name: prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string(),
            source: format!("s3://{bucket}/{prefix}"),
            target: match to {
                Access::PublicRead => t!("access.make_public").into_owned(),
                _ => t!("access.make_private").into_owned(),
            },
        };
        let total = keys.len() as u64;
        cx.global::<AppJobs>().start(label, total, move |job| {
            struct Storage<'a> {
                requests: &'a Requests,
                profile_id: String,
                bucket: String,
                to: Access,
            }
            impl crate::folder_access::AccessBackend for Storage<'_> {
                fn set(&self, key: &str) -> Result<(), String> {
                    let object = ObjectRef {
                        profile_id: self.profile_id.clone(),
                        bucket: self.bucket.clone(),
                        key: key.to_string(),
                    };
                    self.requests
                        .wait(self.requests.set_object_access(object, self.to))
                }
                fn identity(&self) -> Option<String> {
                    let profiles = self.requests.wait(self.requests.profiles()).ok()?;
                    profiles
                        .iter()
                        .find(|profile| profile.id == self.profile_id)
                        .map(|profile| profile.cache_identity())
                }
            }
            let storage = Storage {
                requests: &requests,
                profile_id,
                bucket,
                to,
            };
            let report = crate::folder_access::run(&keys, &identity, &storage, job);
            crate::folder_access::outcome(&report, job.cancelled(), |report| {
                t!(
                    if report.connection_changed {
                        "folder_access.stopped"
                    } else {
                        "folder_access.summary"
                    },
                    changed = report.changed,
                    failed = report.failed,
                    left = report.left,
                    error = report
                        .first_error
                        .as_deref()
                        .map(brief_error)
                        .unwrap_or_default()
                )
                .into_owned()
            })
        });
        Self::notify(
            NotificationType::Info,
            t!("folder_access.started").into_owned(),
            window,
            cx,
        );
    }

    /// Asks for a new bucket's name and makes it.
    fn show_create_bucket(
        &mut self,
        side: Side,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.connections.read(cx).get(&profile_id).cloned() else {
            return;
        };
        let service = Service::of(&profile);
        let region = profile.region.clone().unwrap_or_default();
        let dialog = cx.new(|cx| CreateBucketDialog::new(service, &region, window, cx));
        cx.subscribe_in(
            &dialog,
            window,
            move |workspace, dialog, event, window, cx| {
                let BucketEvent::Create(name, region) = event else {
                    return;
                };
                let Some(requests) = AppBackend::requests(cx) else {
                    return;
                };
                let (dialog, name, region, profile_id) = (
                    dialog.clone(),
                    name.clone(),
                    region.clone(),
                    profile_id.clone(),
                );
                let _ = workspace;
                cx.spawn_in(window, async move |workspace, cx| {
                    let result = requests
                        .create_bucket(profile_id.clone(), name.clone(), region)
                        .await;
                    let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                        Ok(()) => {
                            window.close_dialog(cx);
                            let buckets = Location::Buckets { profile_id };
                            workspace.pane(side).clone().update(cx, |pane, cx| {
                                pane.location_changed(Some(&buckets), true, cx)
                            });
                            let message = t!("bucket.created", name = name).into_owned();
                            Self::notify(NotificationType::Success, message, window, cx);
                        }
                        Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                    });
                })
                .detach();
            },
        )
        .detach();
        let title: SharedString = t!("bucket.new_title").into_owned().into();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .child(dialog.clone())
        });
    }

    /// Asks for a bucket's name to be typed, then deletes the bucket, if
    /// nothing in supaS3 is still working in it.
    fn confirm_delete_bucket(
        &mut self,
        profile_id: String,
        bucket: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unsaved = self.unsaved_editors(cx).into_iter().any(|(_, view)| {
            matches!(view.read(cx).file(), FileRef::Remote(object)
                if object.profile_id == profile_id && object.bucket == bucket)
        });
        let busy = self.task_center.read(cx).active_in_bucket(&bucket);
        if unsaved || busy {
            let key = if unsaved {
                "bucket.delete_unsaved"
            } else {
                "bucket.delete_busy"
            };
            Self::notify(NotificationType::Info, t!(key).into_owned(), window, cx);
            return;
        }
        let dialog = cx.new(|cx| DeleteBucketDialog::new(bucket.clone(), window, cx));
        let title: SharedString = t!("bucket.delete_title", name = bucket.as_str())
            .into_owned()
            .into();
        cx.subscribe_in(&dialog, window, move |_, dialog, event, window, cx| {
            let BucketEvent::Delete(typed) = event else {
                return;
            };
            let Some(requests) = AppBackend::requests(cx) else {
                return;
            };
            let (dialog, typed, profile_id, bucket) = (
                dialog.clone(),
                typed.clone(),
                profile_id.clone(),
                bucket.clone(),
            );
            cx.spawn_in(window, async move |workspace, cx| {
                let result = requests
                    .delete_bucket(profile_id.clone(), bucket.clone(), typed)
                    .await
                    .map_err(|error| {
                        if error.contains("BucketNotEmpty") {
                            t!("bucket.not_empty").into_owned()
                        } else {
                            error
                        }
                    });
                let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                    Ok(()) => {
                        window.close_dialog(cx);
                        workspace.bucket_deleted(&profile_id, &bucket, window, cx);
                    }
                    Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                });
            })
            .detach();
        })
        .detach();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .child(dialog.clone())
        });
    }

    /// A bucket is gone: no tab, history, favorite or recent place leads
    /// there any more, and editors of its objects close.
    fn bucket_deleted(
        &mut self,
        profile_id: &str,
        bucket: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let gone = |location: &Location| location.is_in_bucket(profile_id, bucket);
        let buckets = Location::Buckets {
            profile_id: profile_id.to_string(),
        };
        self.places.update(cx, |places, cx| {
            places.forget_where(gone);
            cx.notify();
        });
        for pane in [self.left.clone(), self.right.clone()] {
            pane.update(cx, |pane, cx| {
                pane.drop_editors(
                    |view, _| {
                        matches!(view.file(), FileRef::Remote(object)
                            if object.profile_id == profile_id && object.bucket == bucket)
                    },
                    cx,
                );
                pane.leave_where(gone, &buckets, cx);
                pane.location_changed(Some(&buckets), true, cx);
            });
        }
        let default = self
            .connections
            .read(cx)
            .get(profile_id)
            .and_then(|profile| profile.compat.default_bucket.clone());
        let message = if default.as_deref() == Some(bucket) {
            t!("bucket.deleted_default", name = bucket).into_owned()
        } else {
            t!("bucket.deleted", name = bucket).into_owned()
        };
        Self::notify(NotificationType::Success, message, window, cx);
        self.refresh_details(cx);
    }

    /// Says what turning versioning on or pausing it means, then does it.
    fn confirm_versioning(
        &mut self,
        profile_id: String,
        bucket: String,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.weak_entity();
        let (title, body, confirm): (SharedString, SharedString, SharedString) = if enabled {
            (
                t!("bucket.versioning_enable_title", name = bucket.as_str())
                    .into_owned()
                    .into(),
                t!("bucket.versioning_enable_body").into_owned().into(),
                t!("bucket.versioning_enable").into_owned().into(),
            )
        } else {
            (
                t!("bucket.versioning_suspend_title", name = bucket.as_str())
                    .into_owned()
                    .into(),
                t!("bucket.versioning_suspend_body").into_owned().into(),
                t!("bucket.versioning_suspend").into_owned().into(),
            )
        };
        window.open_dialog(cx, move |dialog, _, _| {
            let (workspace, profile_id, bucket) =
                (workspace.clone(), profile_id.clone(), bucket.clone());
            dialog
                .title(title.clone())
                // Enter does not change how the bucket keeps versions.
                .on_ok(|_, _, _| false)
                .child(div().text_sm().whitespace_normal().child(body.clone()))
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-versioning")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-versioning")
                                .label(confirm.clone())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let (profile_id, bucket) = (profile_id.clone(), bucket.clone());
                                    let _ = workspace.update(cx, |_, cx| {
                                        let Some(requests) = AppBackend::requests(cx) else {
                                            return;
                                        };
                                        cx.spawn_in(window, async move |workspace, cx| {
                                            let result = requests
                                                .set_bucket_versioning(profile_id, bucket, enabled)
                                                .await;
                                            let _ =
                                                workspace.update_in(cx, |workspace, window, cx| {
                                                    if let Err(error) = result {
                                                        let message = t!(
                                                            "bucket.versioning_failed",
                                                            detail = brief_error(&error)
                                                        )
                                                        .into_owned();
                                                        Self::notify(
                                                            NotificationType::Error,
                                                            message,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                    workspace.details.update(cx, |details, cx| {
                                                        details.reload(cx)
                                                    });
                                                });
                                        })
                                        .detach();
                                    });
                                }),
                        ),
                )
        });
    }

    /// Reads a bucket's policy and opens it for editing.
    fn show_policy(
        &mut self,
        profile_id: String,
        bucket: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn_in(window, async move |_, cx| {
            let read = requests
                .bucket_policy(profile_id.clone(), bucket.clone())
                .await;
            let _ = cx.update(|window, cx| {
                let original = match read {
                    Ok(policy) => policy,
                    Err(error) => {
                        let message =
                            t!("policy.unreadable", detail = brief_error(&error)).into_owned();
                        Self::notify(NotificationType::Error, message, window, cx);
                        return;
                    }
                };
                let dialog = cx.new(|cx| PolicyDialog::new(original, window, cx));
                let requests = requests.clone();
                let subscription_dialog = dialog.clone();
                let (profile_id, bucket2) = (profile_id.clone(), bucket.clone());
                window
                    .subscribe(
                        &subscription_dialog,
                        cx,
                        move |dialog, event: &BucketEvent, window, cx| {
                            let BucketEvent::SavePolicy(policy) = event else {
                                return;
                            };
                            let expected = dialog.read(cx).original();
                            let (requests, policy, profile_id, bucket) = (
                                requests.clone(),
                                policy.clone(),
                                profile_id.clone(),
                                bucket2.clone(),
                            );
                            let dialog = dialog.clone();
                            window
                                .spawn(cx, async move |cx| {
                                    let result = requests
                                        .put_bucket_policy(profile_id, bucket, policy, expected)
                                        .await;
                                    let _ = cx.update(|window, cx| match result {
                                        Ok(()) => {
                                            window.close_dialog(cx);
                                            let message = t!("policy.saved").into_owned();
                                            Self::notify(
                                                NotificationType::Success,
                                                message,
                                                window,
                                                cx,
                                            );
                                        }
                                        Err(error) => {
                                            dialog.update(cx, |dialog, cx| dialog.failed(error, cx))
                                        }
                                    });
                                })
                                .detach();
                        },
                    )
                    .detach();
                let title: SharedString = t!("policy.title", name = bucket.as_str())
                    .into_owned()
                    .into();
                let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
                window.open_dialog(cx, move |modal, _, cx| {
                    let busy = dialog.read(cx).is_busy();
                    modal
                        .title(title.clone())
                        .width(width)
                        .keyboard(!busy)
                        .overlay_closable(!busy)
                        .close_button(!busy)
                        .on_ok(|_, _, _| false)
                        .child(dialog.clone())
                });
            });
        })
        .detach();
    }

    /// Asks for web addresses and queues their import into `location`.
    fn show_import(&mut self, location: Location, window: &mut Window, cx: &mut Context<Self>) {
        let Location::Objects {
            profile_id,
            bucket,
            prefix,
        } = location.clone()
        else {
            return;
        };
        let place = self.describe(&location, cx);
        let dialog = cx.new(|cx| ImportDialog::new(place, window, cx));
        cx.subscribe_in(&dialog, window, move |_, dialog, event, window, cx| {
            let ImportEvent::Submitted(items, max_bytes) = event;
            let Some(requests) = AppBackend::requests(cx) else {
                let message = t!("error.backend_stopped").into_owned();
                dialog.update(cx, |dialog, cx| dialog.failed(message, cx));
                return;
            };
            let (dialog, items, max_bytes) = (dialog.clone(), items.clone(), *max_bytes);
            let (profile_id, bucket, prefix) = (profile_id.clone(), bucket.clone(), prefix.clone());
            cx.spawn_in(window, async move |_, cx| {
                let result = requests
                    .import_urls(profile_id, bucket, prefix, items, max_bytes)
                    .await;
                let _ = cx.update(|window, cx| match result {
                    Ok(count) => {
                        window.close_dialog(cx);
                        let message = t!("import.queued", count = count).into_owned();
                        Self::notify(NotificationType::Info, message, window, cx);
                    }
                    Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                });
            })
            .detach();
        })
        .detach();
        let title: SharedString = t!("import.title").into_owned().into();
        let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .child(dialog.clone())
        });
    }

    /// Edits an object's user metadata, only on the properties the panel
    /// showed.
    fn show_metadata(
        &mut self,
        object: ObjectRef,
        seen: SeenProperties,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let idrive_e2 = self
            .connections
            .read(cx)
            .get(&object.profile_id)
            .is_some_and(|profile| {
                profile.compat.provider == supas3_core::credentials::Provider::IdriveE2
            });
        let original = seen.metadata.clone();
        let dialog = cx.new(|cx| MetadataDialog::new(original, idrive_e2, window, cx));
        let name = object
            .key
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        cx.subscribe_in(&dialog, window, move |_, dialog, event, window, cx| {
            let MetadataEvent::Submitted(metadata) = event;
            let Some(requests) = AppBackend::requests(cx) else {
                let message = t!("error.backend_stopped").into_owned();
                dialog.update(cx, |dialog, cx| dialog.failed(message, cx));
                return;
            };
            let (dialog, object, metadata, seen) = (
                dialog.clone(),
                object.clone(),
                metadata.clone(),
                seen.clone(),
            );
            cx.spawn_in(window, async move |workspace, cx| {
                let result = requests
                    .set_metadata(object.clone(), metadata, seen)
                    .await
                    .map_err(|error| {
                        // The refusals made before anything is written.
                        if error.contains("changed since") {
                            t!("metadata.changed").into_owned()
                        } else if error.contains("only for objects up to 5 GiB") {
                            t!("properties.too_large").into_owned()
                        } else {
                            error
                        }
                    });
                let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                    Ok(()) => {
                        window.close_dialog(cx);
                        workspace.object_changed(&object, cx);
                    }
                    Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                });
            })
            .detach();
        })
        .detach();
        let title: SharedString = t!("metadata.title", name = name).into_owned().into();
        let width = rems(WIDE_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .child(dialog.clone())
        });
    }

    /// Confirms giving an object a new ACL, saying what it means, then
    /// makes the change.
    fn confirm_access(
        &mut self,
        object: ObjectRef,
        to: Access,
        current: Access,
        etag: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.weak_entity();
        let name = object
            .key
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let (title, confirm) = match to {
            Access::PublicRead => (
                t!("access.public_title", name = name),
                t!("access.make_public"),
            ),
            _ => (
                t!("access.private_title", name = name),
                t!("access.make_private"),
            ),
        };
        // Only Alibaba Cloud OSS has objects follow their bucket's ACL.
        let on_oss = self
            .connections
            .read(cx)
            .get(&object.profile_id)
            .is_some_and(|profile| Service::of(profile) == Service::Oss);
        let mut lines = vec![t!(if to == Access::PublicRead {
            "access.public_body"
        } else {
            "access.private_body"
        })
        .into_owned()];
        if current == Access::Custom {
            lines.push(t!("access.custom_replaced").into_owned());
        }
        if on_oss {
            lines.push(t!("access.inherit_note").into_owned());
        }
        let (title, confirm): (SharedString, SharedString) =
            (title.into_owned().into(), confirm.into_owned().into());
        let address = object.display();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (workspace, object, etag) = (workspace.clone(), object.clone(), etag.clone());
            let muted = cx.theme().muted_foreground;
            dialog
                .title(title.clone())
                // Enter does not change who may read data.
                .on_ok(|_, _, _| false)
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_xs().text_color(muted).child(address.clone()))
                        .children(lines.iter().map(|line| div().text_sm().child(line.clone()))),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-access")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-access")
                                .when(to == Access::PublicRead, |this| this.danger())
                                .label(confirm.clone())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let (object, etag) = (object.clone(), etag.clone());
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        workspace.set_access(object, to, current, etag, window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    /// Gives an object a new ACL, if it still has the ACL and the content
    /// the panel showed. The check and the change are separate requests.
    fn set_access(
        &mut self,
        object: ObjectRef,
        to: Access,
        current: Access,
        etag: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            Self::notify(NotificationType::Error, message, window, cx);
            return;
        };
        let custom_endpoint = self
            .connections
            .read(cx)
            .get(&object.profile_id)
            .is_some_and(|profile| Service::of(profile) != Service::Aws);
        self.details
            .update(cx, |details, cx| details.access_changing(cx));
        cx.spawn_in(window, async move |workspace, cx| {
            let result = async {
                let now = requests.object_permissions(object.clone()).await?;
                let grants: Vec<Grant> = now
                    .grants
                    .iter()
                    .map(|grant| Grant {
                        grantee_type: grant.grantee_type.as_deref(),
                        id: grant.id.as_deref(),
                        uri: grant.uri.as_deref(),
                        permission: grant.permission.as_deref(),
                    })
                    .collect();
                if access_of(now.owner_id.as_deref(), &grants, custom_endpoint) != current {
                    return Err(t!("access.changed_meanwhile").into_owned());
                }
                if let Some(etag) = etag {
                    let shown = requests.object_properties(object.clone()).await?.e_tag;
                    if shown.as_deref() != Some(etag.as_str()) {
                        return Err(t!("access.object_changed").into_owned());
                    }
                }
                requests.set_object_access(object.clone(), to).await
            }
            .await;
            let _ = workspace.update_in(cx, |workspace, window, cx| {
                if let Err(error) = result {
                    let message = t!("access.failed", detail = brief_error(&error)).into_owned();
                    Self::notify(NotificationType::Error, message, window, cx);
                }
                workspace.object_changed(&object, cx);
            });
        })
        .detach();
    }

    /// Downloads one row: into the other pane when it shows a folder on
    /// this computer, otherwise into a folder the user picks.
    fn download(
        &mut self,
        source: Location,
        entry: Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let other = self
            .pane(Self::other(self.active))
            .read(cx)
            .location()
            .clone();
        if let Location::Local(_) = other {
            self.start_copy(
                plan(&source, std::slice::from_ref(&entry), &other),
                window,
                cx,
            );
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("details.download_here").into_owned().into()),
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            let _ = workspace.update_in(cx, |workspace, window, cx| {
                let target = Location::Local(folder);
                workspace.start_copy(
                    plan(&source, std::slice::from_ref(&entry), &target),
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// Shows the sidebar as icons only, or in full.
    pub fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Folded only for want of room: open or fold it for now, leaving
        // the preference as it is.
        if !self.settings.sidebar_collapsed && self.narrow_with_details(window) {
            self.sidebar_unsqueezed = !self.sidebar_unsqueezed;
            cx.notify();
            return;
        }
        self.store_settings(
            |settings| settings.sidebar_collapsed = !settings.sidebar_collapsed,
            window,
            cx,
        );
    }

    /// Changes a preference that only affects the layout and saves it.
    /// Unlike [`Self::change_settings`], the tables are left as they are, so
    /// their column widths stay.
    fn store_settings(
        &mut self,
        change: impl FnOnce(&mut Settings),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        change(&mut self.settings);
        let saved = default_config_dir()
            .map_err(|error| error.to_string())
            .and_then(|dir| self.settings.save(&dir).map_err(|error| error.to_string()));
        if let Err(error) = saved {
            log::warn!("could not save interface preferences: {error}");
            window.push_notification(
                (
                    NotificationType::Warning,
                    SharedString::from(t!("preferences.not_saved").into_owned()),
                ),
                cx,
            );
        }
        cx.notify();
    }

    /// Asks for the transfer limits, applies them and saves them.
    fn show_transfer_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.settings.transfers;
        let dialog = cx.new(|cx| TransfersDialog::new(current, window, cx));
        cx.subscribe_in(&dialog, window, |_, dialog, event, window, cx| {
            let TransfersDialogEvent::Submitted(limits) = *event;
            let Some(requests) = AppBackend::requests(cx) else {
                let message = t!("error.backend_stopped").into_owned();
                dialog.update(cx, |dialog, cx| dialog.failed(message, cx));
                return;
            };
            let dialog = dialog.clone();
            cx.spawn_in(window, async move |workspace, cx| {
                let result = requests.set_transfer_limits(limits).await;
                let _ = workspace.update_in(cx, |workspace, window, cx| match result {
                    Ok(()) => {
                        window.close_dialog(cx);
                        workspace.store_settings(|s| s.transfers = limits, window, cx);
                    }
                    Err(error) => dialog.update(cx, |dialog, cx| dialog.failed(error, cx)),
                });
            })
            .detach();
        })
        .detach();
        let title: SharedString = t!("transfers.title").into_owned().into();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |modal, _, cx| {
            let busy = dialog.read(cx).is_busy();
            modal
                .title(title.clone())
                .width(width)
                .keyboard(!busy)
                .overlay_closable(!busy)
                .close_button(!busy)
                .on_ok(|_, _, _| false)
                .child(dialog.clone())
        });
    }

    pub fn show_about(window: &mut Window, cx: &mut App) {
        const SOURCE: &str = env!("CARGO_PKG_REPOSITORY");
        const UPSTREAM: &str = "https://github.com/rgcsekaraa/brows3";
        const LICENSE_TEXT: &str = "https://www.gnu.org/licenses/agpl-3.0.html";
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            dialog
                .title(t!("about.title").into_owned())
                .width(width)
                .child(
                    v_flex()
                        .gap_3()
                        .text_sm()
                        .child(
                            t!("about.version", version = env!("CARGO_PKG_VERSION")).into_owned(),
                        )
                        .child(
                            div()
                                .text_color(muted)
                                .child(t!("app.tagline").into_owned()),
                        )
                        .child(t!("about.copyright").into_owned())
                        .child(t!("about.license").into_owned())
                        .child(
                            h_flex()
                                .gap_4()
                                .child(
                                    Link::new("about-license")
                                        .href(LICENSE_TEXT)
                                        .child(t!("about.license_text").into_owned()),
                                )
                                .child(
                                    Link::new("about-source")
                                        .href(SOURCE)
                                        .child(t!("about.source").into_owned()),
                                ),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(t!("about.upstream").into_owned())
                                .child(Link::new("about-upstream").href(UPSTREAM).child(UPSTREAM)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("about.notices").into_owned()),
                        ),
                )
                .footer(
                    h_flex().justify_end().child(
                        Button::new("close-about")
                            .outline()
                            .label(t!("action.close").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    fn open_in_active(&mut self, location: Location, cx: &mut Context<Self>) {
        self.pane(self.active)
            .clone()
            .update(cx, |pane, cx| pane.open(location, cx));
    }

    fn show_form(
        &mut self,
        existing: Option<Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title: SharedString = match &existing {
            Some(profile) => t!("connection.edit_title", name = profile.name)
                .into_owned()
                .into(),
            None => t!("connection.add_title").into_owned().into(),
        };
        let editing = existing.is_some();
        let form = cx.new(|cx| ConnectionForm::new(existing, window, cx));
        cx.subscribe_in(&form, window, move |workspace, form, event, window, cx| {
            // The dialog on top belongs to this form only while the workspace
            // still holds it; an answer that arrives after the form was
            // closed must not close whatever is open now.
            let is_open = workspace
                .form
                .as_ref()
                .is_some_and(|open| open.entity_id() == form.entity_id());
            if is_open {
                window.close_dialog(cx);
                workspace.form = None;
            }
            if let ConnectionFormEvent::Saved(profile) = event {
                workspace.reload_connections(false, cx);
                if editing {
                    // Tabs already showing this connection hold rows and
                    // paging tokens fetched with the old settings.
                    for pane in [workspace.left.clone(), workspace.right.clone()] {
                        pane.update(cx, |pane, cx| pane.reload_profile(&profile.id, cx));
                    }
                } else if is_open {
                    workspace.open_in_active(start_of(profile), cx);
                }
            }
        })
        .detach();
        self.form = Some(form.clone());
        let workspace = cx.weak_entity();
        let width = rems(FORM_WIDTH).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, _| {
            let submit = form.clone();
            let (closed_form, closed_workspace) = (form.clone(), workspace.clone());
            dialog
                .title(title.clone())
                .width(width)
                .child(form.clone())
                // Enter saves through the form, which closes the dialog only
                // after the connection has been stored.
                .on_ok(move |_, _, cx| {
                    submit.update(cx, |form, cx| form.save(cx));
                    false
                })
                // Escape, the close button and a click outside all end here:
                // let go of the form so it is not kept alive behind the scenes.
                .on_close(move |_, _, cx| {
                    let _ = closed_workspace.update(cx, |workspace, _| {
                        let is_this = workspace
                            .form
                            .as_ref()
                            .is_some_and(|open| open.entity_id() == closed_form.entity_id());
                        if is_this {
                            workspace.form = None;
                        }
                    });
                })
        });
    }

    fn confirm_delete(&mut self, profile: Profile, window: &mut Window, cx: &mut Context<Self>) {
        // Edits to its objects would have nowhere to be saved.
        let unsaved = self
            .unsaved_editors(cx)
            .into_iter()
            .any(|(_, view)| editor_profile(&view, cx) == Some(profile.id.clone()));
        if unsaved {
            let message = t!("connection.unsaved_edits").into_owned();
            Self::notify(NotificationType::Info, message, window, cx);
            return;
        }
        let workspace = cx.weak_entity();
        let title: SharedString = t!("connection.delete_title", name = profile.name)
            .into_owned()
            .into();
        window.open_dialog(cx, move |dialog, _, _| {
            let workspace = workspace.clone();
            let profile_id = profile.id.clone();
            dialog
                .title(title.clone())
                .child(t!("connection.delete_body").into_owned())
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-delete")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-delete")
                                .danger()
                                .label(t!("action.delete").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let profile_id = profile_id.clone();
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        workspace.delete_connection(profile_id, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn delete_connection(&mut self, profile_id: String, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn(async move |workspace, cx| {
            let result = requests.delete_profile(profile_id.clone()).await;
            let _ = workspace.update(cx, |workspace, cx| match result {
                Ok(()) => {
                    // Tabs showing the removed connection have nothing to show.
                    for pane in [workspace.left.clone(), workspace.right.clone()] {
                        pane.update(cx, |pane, cx| {
                            pane.leave_profile(&profile_id, &home(), cx);
                            // Files edited meanwhile stay open with their
                            // text; only the others go.
                            pane.drop_editors(
                                |view, cx| {
                                    matches!(view.file(), FileRef::Remote(object)
                                        if object.profile_id == profile_id)
                                        && !view.holds_work(cx)
                                },
                                cx,
                            );
                        });
                    }
                    workspace.places.update(cx, |places, cx| {
                        places.forget_profile(&profile_id);
                        cx.notify();
                    });
                    workspace.reload_connections(false, cx);
                }
                Err(error) => {
                    workspace.notice =
                        Some(t!("connection.delete_failed", detail = error).into_owned());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_appearance(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.settings.appearance {
            Appearance::System => Theme::sync_system_appearance(Some(window), cx),
            Appearance::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            Appearance::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
    }

    fn change_settings(
        &mut self,
        change: impl FnOnce(&mut Settings),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        change(&mut self.settings);
        i18n::apply(self.settings.language);
        crate::set_app_menus(cx);
        self.apply_appearance(window, cx);
        // Column titles are fetched once per table; fetch them again.
        for pane in [self.left.clone(), self.right.clone()] {
            pane.update(cx, |pane, cx| pane.refresh_labels(cx));
        }
        let saved = default_config_dir()
            .map_err(|error| error.to_string())
            .and_then(|dir| self.settings.save(&dir).map_err(|error| error.to_string()));
        if let Err(error) = saved {
            log::warn!("could not save interface preferences: {error}");
            // The choice is applied; say that it will not survive a restart.
            window.push_notification(
                (
                    NotificationType::Warning,
                    SharedString::from(t!("preferences.not_saved").into_owned()),
                ),
                cx,
            );
        }
        cx.refresh_windows();
    }

    /// Runs a command of the menu or a shortcut on the active pane. Nothing
    /// happens behind an open dialog.
    pub fn pane_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let pane = self.pane(self.active).clone();
        pane.update(cx, |pane, cx| match command {
            PaneCommand::NewFolder => pane.new_folder(cx),
            PaneCommand::Rename => pane.rename_selected(cx),
            PaneCommand::Filter => pane.open_filter(window, cx),
            PaneCommand::Copy => pane.copy_selection(cx),
            PaneCommand::Move => pane.move_selection(cx),
        });
    }

    /// Tab and Shift+Tab move the keyboard between the sidebar and the two
    /// panes.
    fn focus_region(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        // 0 is the sidebar, 1 the left pane, 2 the right pane.
        let current = if self.sidebar_focus.contains_focused(window, cx) {
            Some(0)
        } else if self.left.read(cx).contains_focus(window, cx) {
            Some(1)
        } else if self.right.read(cx).contains_focus(window, cx) {
            Some(2)
        } else {
            None
        };
        let next = match (current, backwards) {
            (None, false) => 0,
            (None, true) => 2,
            (Some(at), false) => (at + 1) % 3,
            (Some(at), true) => (at + 2) % 3,
        };
        match next {
            0 => window.focus(&self.sidebar_focus, cx),
            side => {
                let side = if side == 1 { Side::Left } else { Side::Right };
                if self.activate(side, cx) {
                    self.remember_layout(side, cx);
                    self.refresh_details(cx);
                }
                self.pane(side)
                    .clone()
                    .update(cx, |pane, cx| pane.focus(window, cx));
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The sidebar's rows, in the order the keyboard moves through them.
    fn sidebar_rows(&self, cx: &App) -> Vec<SidebarRow> {
        let places = self.places.read(cx);
        let mut rows = vec![SidebarRow::Home];
        rows.extend(places.favorites.iter().cloned().map(SidebarRow::Favorite));
        // Favorites are already one click away; recent lists the others.
        rows.extend(
            places
                .recent
                .iter()
                .filter(|location| !places.favorites.contains(location))
                .take(5)
                .cloned()
                .map(SidebarRow::Recent),
        );
        rows.extend(
            self.connections
                .read(cx)
                .profiles()
                .iter()
                .cloned()
                .map(|profile| SidebarRow::Connection(Box::new(profile))),
        );
        rows
    }

    /// A row's label, icon and the full name of what it opens.
    fn sidebar_row_text(&self, row: &SidebarRow, cx: &App) -> (String, IconName, String) {
        let place = |location: &Location, icon: IconName| {
            let connection = location
                .profile_id()
                .map(|id| self.connections.read(cx).name_of(id))
                .unwrap_or_default();
            (
                short_name(location, &connection),
                icon,
                full_name(location, &connection),
            )
        };
        match row {
            SidebarRow::Home => {
                let label = t!("location.home").into_owned();
                (label.clone(), IconName::HardDrive, label)
            }
            SidebarRow::Favorite(location) => place(location, IconName::Star),
            SidebarRow::Recent(location) => place(
                location,
                match location {
                    Location::Local(_) => IconName::Folder,
                    _ => IconName::Globe,
                },
            ),
            SidebarRow::Connection(profile) => {
                (profile.name.clone(), IconName::Globe, profile.name.clone())
            }
        }
    }

    fn open_sidebar_row(&mut self, row: &SidebarRow, cx: &mut Context<Self>) {
        let location = match row {
            SidebarRow::Home => home(),
            SidebarRow::Favorite(location) | SidebarRow::Recent(location) => location.clone(),
            SidebarRow::Connection(profile) => start_of(profile),
        };
        self.open_in_active(location, cx);
    }

    /// Up and Down move through the rows, Home and End go to either end,
    /// Enter or Space opens one, and Shift+F10 or the Menu key lists what
    /// can be done with it.
    fn on_sidebar_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.sidebar_rows(cx);
        if rows.is_empty() {
            return;
        }
        let last = rows.len() - 1;
        let keystroke = &event.keystroke;
        let plain = !keystroke.modifiers.modified();
        let cursor = self.sidebar_cursor.min(last);
        match keystroke.key.as_str() {
            "down" if plain => self.sidebar_cursor = (cursor + 1).min(last),
            "up" if plain => self.sidebar_cursor = cursor.saturating_sub(1),
            "home" if plain => self.sidebar_cursor = 0,
            "end" if plain => self.sidebar_cursor = last,
            "enter" | "space" if plain => {
                let row = rows[cursor].clone();
                self.open_sidebar_row(&row, cx);
            }
            "f10" if keystroke.modifiers.shift => {
                self.show_sidebar_row_actions(rows[cursor].clone(), window, cx);
            }
            "menu" | "contextmenu" => {
                self.show_sidebar_row_actions(rows[cursor].clone(), window, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// What can be done with a row, for the keyboard: the same choices as
    /// its context menu, as buttons.
    fn show_sidebar_row_actions(
        &mut self,
        row: SidebarRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (_, _, full) = self.sidebar_row_text(&row, cx);
        let title: SharedString = full.into();
        let workspace = cx.weak_entity();
        let places = self.places.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let open = {
                let (workspace, row) = (workspace.clone(), row.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    window.close_dialog(cx);
                    let _ =
                        workspace.update(cx, |workspace, cx| workspace.open_sidebar_row(&row, cx));
                }
            };
            let mut buttons = v_flex().gap_2().child(
                Button::new("row-open")
                    .primary()
                    .w_full()
                    .label(t!("action.open").into_owned())
                    .on_click(open),
            );
            match &row {
                SidebarRow::Connection(profile) => {
                    let (edit, delete) = ((**profile).clone(), (**profile).clone());
                    let (edit_workspace, delete_workspace) = (workspace.clone(), workspace.clone());
                    buttons = buttons
                        .child(
                            Button::new("row-edit")
                                .outline()
                                .w_full()
                                .label(t!("connection.edit").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let profile = edit.clone();
                                    let _ = edit_workspace.update(cx, |workspace, cx| {
                                        workspace.show_form(Some(profile), window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("row-delete")
                                .outline()
                                .w_full()
                                .label(t!("connection.delete").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let profile = delete.clone();
                                    let _ = delete_workspace.update(cx, |workspace, cx| {
                                        workspace.confirm_delete(profile, window, cx);
                                    });
                                }),
                        );
                }
                SidebarRow::Favorite(location) => {
                    let (places, removed) = (places.clone(), location.clone());
                    buttons = buttons.child(
                        Button::new("row-unfavorite")
                            .outline()
                            .w_full()
                            .label(t!("places.remove_favorite").into_owned())
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                places.update(cx, |places, cx| {
                                    places.toggle_favorite(&removed);
                                    cx.notify();
                                });
                            }),
                    );
                }
                SidebarRow::Home | SidebarRow::Recent(_) => {}
            }
            dialog.title(title.clone()).child(buttons)
        });
    }

    /// One row of the sidebar.
    fn render_sidebar_row(
        &self,
        index: usize,
        row: &SidebarRow,
        active_location: &Location,
        keyboard_here: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.sidebar_collapsed();
        let (label, icon, full) = self.sidebar_row_text(row, cx);
        let active = match row {
            SidebarRow::Home => matches!(active_location, Location::Local(_)),
            SidebarRow::Favorite(location) | SidebarRow::Recent(location) => {
                location == active_location
            }
            SidebarRow::Connection(profile) => {
                active_location.profile_id() == Some(profile.id.as_str())
            }
        };
        let theme = cx.theme();
        let (accent, accent_text, ring) = (
            theme.sidebar_accent,
            theme.sidebar_accent_foreground,
            theme.ring,
        );
        let clicked = row.clone();
        let tooltip: SharedString = full.clone().into();
        let mut element = h_flex()
            .id(("sidebar-row", index))
            .h_8()
            .px_2()
            .gap_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(if keyboard_here {
                ring
            } else {
                theme.transparent
            })
            .when(collapsed, |this| this.justify_center())
            .when(active, |this| this.bg(accent).text_color(accent_text))
            // Hovering is lighter than being where the pane is.
            .when(!active, |this| {
                this.hover(move |style| style.bg(accent.opacity(0.5)))
            })
            .child(
                gpui_kit::component::Icon::new(icon)
                    .size_4()
                    .flex_shrink_0(),
            )
            .when(!collapsed, |this| {
                this.child(div().flex_1().min_w_0().truncate().text_sm().child(label))
            })
            .when(collapsed, |this| {
                this.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            })
            .on_click(cx.listener(move |workspace, _, _, cx| {
                workspace.sidebar_cursor = index;
                workspace.open_sidebar_row(&clicked, cx);
            }));
        if let (SidebarRow::Connection(profile), false) = (row, collapsed) {
            let profile = (**profile).clone();
            let workspace = cx.weak_entity();
            element = element.child(
                Button::new(SharedString::from(format!("actions-{}", profile.id)))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .tooltip(t!("connection.actions", name = profile.name).into_owned())
                    .dropdown_menu(move |menu, _, _| {
                        let (edit_profile, delete_profile) = (profile.clone(), profile.clone());
                        let (edit_workspace, delete_workspace) =
                            (workspace.clone(), workspace.clone());
                        menu.item(
                            PopupMenuItem::new(t!("connection.edit").into_owned()).on_click(
                                move |_, window, cx| {
                                    let profile = edit_profile.clone();
                                    let _ = edit_workspace.update(cx, |workspace, cx| {
                                        workspace.show_form(Some(profile), window, cx);
                                    });
                                },
                            ),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new(t!("connection.delete").into_owned()).on_click(
                                move |_, window, cx| {
                                    let profile = delete_profile.clone();
                                    let _ = delete_workspace.update(cx, |workspace, cx| {
                                        workspace.confirm_delete(profile, window, cx);
                                    });
                                },
                            ),
                        )
                    }),
            );
        }
        // The full place and removal are in the row's context menu, so
        // nothing inside the row competes with opening it.
        match row {
            SidebarRow::Favorite(location) | SidebarRow::Recent(location) => {
                let favorite = matches!(row, SidebarRow::Favorite(_));
                let (places, removed) = (self.places.clone(), location.clone());
                let full: SharedString = full.into();
                element
                    .context_menu(move |menu, _, _| {
                        let (places, removed) = (places.clone(), removed.clone());
                        let menu = menu.label(full.clone());
                        if favorite {
                            menu.separator().item(
                                PopupMenuItem::new(t!("places.remove_favorite").into_owned())
                                    .on_click(move |_, _, cx| {
                                        let removed = removed.clone();
                                        places.update(cx, |places, cx| {
                                            places.toggle_favorite(&removed);
                                            cx.notify();
                                        });
                                    }),
                            )
                        } else {
                            menu
                        }
                    })
                    .into_any_element()
            }
            _ => element.into_any_element(),
        }
    }

    /// Whether the sidebar shows icons only: as chosen, or for want of room.
    /// Whether the details panel is open in a window too narrow for it, the
    /// two panes and the full sidebar together.
    fn narrow_with_details(&self, window: &Window) -> bool {
        self.settings.details_open
            && window.viewport_size().width < rems(90.).to_pixels(window.rem_size())
    }

    fn sidebar_collapsed(&self) -> bool {
        self.settings.sidebar_collapsed || self.sidebar_squeezed
    }

    fn render_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_location = self.pane(self.active).read(cx).location().clone();
        let settings = self.settings;
        let collapsed = self.sidebar_collapsed();
        let workspace = cx.weak_entity();
        let rows = self.sidebar_rows(cx);
        let keyboard = self.sidebar_focus.is_focused(window);
        let cursor = self.sidebar_cursor.min(rows.len().saturating_sub(1));
        let muted = cx.theme().muted_foreground;
        let group = |label: String| {
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .text_xs()
                .text_color(muted)
                .child(label)
        };
        let mut list = v_flex()
            .id("sidebar-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .gap_0p5();
        let mut previous: Option<&'static str> = None;
        for (index, row) in rows.iter().enumerate() {
            let heading = match row {
                SidebarRow::Home => "sidebar.places",
                SidebarRow::Favorite(_) => "sidebar.favorites",
                SidebarRow::Recent(_) => "sidebar.recent",
                SidebarRow::Connection(_) => "sidebar.connections",
            };
            if previous != Some(heading) {
                if heading == "sidebar.connections" || previous.is_none() || !collapsed {
                    list = list.child(if collapsed {
                        div().h_2().into_any_element()
                    } else {
                        group(t!(heading).into_owned()).into_any_element()
                    });
                }
                previous = Some(heading);
            }
            let here = keyboard && index == cursor;
            list = list.child(self.render_sidebar_row(index, row, &active_location, here, cx));
        }
        if !rows
            .iter()
            .any(|row| matches!(row, SidebarRow::Connection(_)))
            && !collapsed
        {
            list = list.child(group(t!("sidebar.connections").into_owned()));
        }
        let has_connections = !self.connections.read(cx).profiles().is_empty();
        let toggle_label = if collapsed {
            t!("sidebar.expand").into_owned()
        } else {
            t!("sidebar.collapse").into_owned()
        };
        let toggle = SidebarToggleButton::new()
            .collapsed(collapsed)
            .accessibility_label(toggle_label)
            .on_click(cx.listener(|workspace, _, window, cx| workspace.toggle_sidebar(window, cx)));
        let theme = cx.theme();
        v_flex()
            .id("sidebar")
            .track_focus(&self.sidebar_focus)
            .key_context("Sidebar")
            .on_key_down(cx.listener(Self::on_sidebar_key))
            .h_full()
            .flex_shrink_0()
            .w(if collapsed { rems(3.) } else { rems(15.) })
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .py_2()
            .child(
                h_flex()
                    .px_2()
                    .pb_1()
                    .when(collapsed, |this| this.justify_center())
                    .when(!collapsed, |this| this.justify_between())
                    .when(!collapsed, |this| {
                        this.child(div().px_2().py_1().font_semibold().child("supaS3"))
                    })
                    .child(toggle),
            )
            .child(list)
            .child(
                v_flex()
                    .px_2()
                    .pt_2()
                    .w_full()
                    .gap_2()
                    .when(!has_connections && !collapsed, |this| {
                        this.child(
                            div()
                                .px_2()
                                .text_sm()
                                .text_color(muted)
                                .child(t!("sidebar.no_connections").into_owned()),
                        )
                    })
                    .child(
                        Button::new("add-connection")
                            .w_full()
                            .icon(IconName::Plus)
                            .when(!collapsed, |this| {
                                this.label(t!("sidebar.add_connection").into_owned())
                            })
                            .when(collapsed, |this| {
                                this.tooltip(t!("sidebar.add_connection").into_owned())
                            })
                            .on_click(cx.listener(|workspace, _, window, cx| {
                                workspace.show_form(None, window, cx);
                            })),
                    )
                    .child(self.preferences_button(workspace, settings, collapsed)),
            )
    }

    fn preferences_button(
        &self,
        workspace: WeakEntity<Self>,
        settings: Settings,
        collapsed: bool,
    ) -> impl IntoElement {
        Button::new("preferences")
            .w_full()
            .ghost()
            .icon(IconName::Settings)
            .when(!collapsed, |this| {
                this.label(t!("sidebar.preferences").into_owned())
            })
            .when(collapsed, |this| {
                this.tooltip(t!("sidebar.preferences").into_owned())
            })
            .dropdown_menu(move |menu, _, _| {
                let pick = |workspace: &WeakEntity<Workspace>, change: fn(&mut Settings)| {
                    let workspace = workspace.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = workspace.update(cx, |workspace, cx| {
                            workspace.change_settings(change, window, cx);
                        });
                    }
                };
                menu.label(t!("preferences.language").into_owned())
                    .item(
                        PopupMenuItem::new(t!("preferences.follow_system").into_owned())
                            .checked(settings.language == Language::System)
                            .on_click(pick(&workspace, |s| s.language = Language::System)),
                    )
                    .item(
                        PopupMenuItem::new("English")
                            .checked(settings.language == Language::English)
                            .on_click(pick(&workspace, |s| s.language = Language::English)),
                    )
                    .item(
                        PopupMenuItem::new("简体中文")
                            .checked(settings.language == Language::SimplifiedChinese)
                            .on_click(pick(&workspace, |s| {
                                s.language = Language::SimplifiedChinese
                            })),
                    )
                    .separator()
                    .label(t!("preferences.appearance").into_owned())
                    .item(
                        PopupMenuItem::new(t!("preferences.follow_system").into_owned())
                            .checked(settings.appearance == Appearance::System)
                            .on_click(pick(&workspace, |s| s.appearance = Appearance::System)),
                    )
                    .item(
                        PopupMenuItem::new(t!("preferences.light").into_owned())
                            .checked(settings.appearance == Appearance::Light)
                            .on_click(pick(&workspace, |s| s.appearance = Appearance::Light)),
                    )
                    .item(
                        PopupMenuItem::new(t!("preferences.dark").into_owned())
                            .checked(settings.appearance == Appearance::Dark)
                            .on_click(pick(&workspace, |s| s.appearance = Appearance::Dark)),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new(t!("preferences.transfers").into_owned()).on_click({
                            let workspace = workspace.clone();
                            move |_, window, cx| {
                                let _ = workspace.update(cx, |workspace, cx| {
                                    workspace.show_transfer_settings(window, cx)
                                });
                            }
                        }),
                    )
                    .item(
                        PopupMenuItem::new(t!("about.menu").into_owned())
                            .on_click(|_, window, cx| Self::show_about(window, cx)),
                    )
            })
    }

    fn render_pane(&self, side: Side, cx: &mut Context<Self>) -> impl IntoElement {
        // The active pane is outlined; sidebar choices open there. The
        // focus ring's color stays for the keyboard focus.
        let outline = if self.active == side {
            cx.theme().list_active_border
        } else {
            cx.theme().transparent
        };
        div()
            .size_full()
            .border_1()
            .border_color(outline)
            .child(self.pane(side).clone())
    }
}

/// For launch checks: SUPAS3_LAUNCH_REPORT names a file written once the
/// main window has drawn its first frame, which a check can wait for on
/// systems where the window itself cannot be inspected.
fn report_first_frame() {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if let Some(path) = std::env::var_os("SUPAS3_LAUNCH_REPORT") {
        let _ = std::fs::write(path, "drawn\n");
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Painting follows this render; report once it is on screen.
        window.on_next_frame(|_, _| report_first_frame());
        // Narrower than this, a pane's navigation bar no longer fits.
        let min_pane = rems(18.).to_pixels(window.rem_size());
        let details_open = self.settings.details_open;
        // Two panes, the details panel and the full sidebar need about
        // 90rem to leave each pane room; below that the sidebar gives way
        // first.
        let narrow = self.narrow_with_details(window);
        if !narrow {
            self.sidebar_unsqueezed = false;
        }
        self.sidebar_squeezed = narrow && !self.sidebar_unsqueezed;
        let details_width = rems(20.).to_pixels(window.rem_size());
        let (min_details, max_details) = (
            rems(15.).to_pixels(window.rem_size()),
            rems(36.).to_pixels(window.rem_size()),
        );
        h_flex()
            .items_stretch()
            .size_full()
            // Tab is bound here, closer than the window's own Tab.
            .key_context("Workspace")
            .on_action(cx.listener(|workspace, _: &FocusNextRegion, window, cx| {
                workspace.focus_region(false, window, cx);
            }))
            .on_action(
                cx.listener(|workspace, _: &FocusPreviousRegion, window, cx| {
                    workspace.focus_region(true, window, cx);
                }),
            )
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .when_some(self.notice.clone(), |this, notice| {
                        this.child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(notice),
                        )
                    })
                    .when_some(self.storage_notice.clone(), |this, notice| {
                        this.child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(notice),
                        )
                    })
                    .child(
                        div().flex_1().min_h_0().child(
                            // Panel sizes are kept per arrangement, so the
                            // panes do not inherit the details panel's width.
                            h_resizable(if details_open {
                                "panes-details"
                            } else {
                                "panes"
                            })
                            .child(
                                resizable_panel()
                                    .size_range(min_pane..Pixels::MAX)
                                    .child(self.render_pane(Side::Left, cx)),
                            )
                            .child(
                                resizable_panel()
                                    .size_range(min_pane..Pixels::MAX)
                                    .child(self.render_pane(Side::Right, cx)),
                            )
                            .when(details_open, |this| {
                                this.child(
                                    resizable_panel()
                                        .size(details_width)
                                        .size_range(min_details..max_details)
                                        .child(self.details.clone()),
                                )
                            }),
                        ),
                    )
                    .child(self.task_center.clone()),
            )
    }
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
