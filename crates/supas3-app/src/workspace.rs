// SPDX-License-Identifier: AGPL-3.0-or-later
//! The main window: connections on the side, two panes of tabs beside them.

use crate::app_state::{AppBackend, Connections};
use crate::backend::default_config_dir;
use crate::connection_form::{ConnectionForm, ConnectionFormEvent};
use crate::entries_table::RowAction;
use crate::i18n;
use crate::listing::Entry;
use crate::location::Location;
use crate::object_actions::{
    file_of, link_may_expire_early, plain_address_is_known, sync_pair, FileRef,
};
use crate::object_view::{ObjectView, ObjectViewEvent};
use crate::pane::{Pane, PaneEvent};
use crate::places::{full_name, short_name, Layout, PaneLayout, Places};
use crate::properties_view::PropertiesView;
use crate::settings::{Appearance, Language, Settings};
use crate::share_view::ShareView;
use crate::sync_view::SyncView;
use crate::task_center::{TaskCenter, TaskCenterEvent};
use crate::tasks::EventQueue;
use crate::transfer_plan::{plan, plan_deletion, plan_drop, Deletion, Plan};
use crate::versions_view::VersionsView;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::sidebar::{Sidebar, SidebarGroup, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, IconName, Sizable as _, StyledExt as _, Theme, ThemeMode,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
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
    /// The pane that sidebar choices open in.
    active: Side,
    settings: Settings,
    /// Kept alive while its dialog is open.
    form: Option<Entity<ConnectionForm>>,
    /// A problem that is not tied to one pane, shown above the panes.
    notice: Option<String>,
    /// Places could not be saved; said once, not on every navigation.
    places_unsaved: bool,
    /// Counts connection reloads, so only the latest answer is applied.
    reload_generation: u64,
    _subscriptions: Vec<Subscription>,
}

/// The folder a local pane starts in.
fn home() -> Location {
    Location::Local(dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")))
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
        let subscriptions = vec![
            cx.subscribe_in(&left, window, |workspace, _, event, window, cx| {
                workspace.on_pane_event(Side::Left, event, window, cx);
            }),
            cx.subscribe_in(&right, window, |workspace, _, event, window, cx| {
                workspace.on_pane_event(Side::Right, event, window, cx);
            }),
            // A finished transfer may have changed what a pane shows.
            cx.subscribe(&task_center, |workspace, _, _: &TaskCenterEvent, cx| {
                for pane in [workspace.left.clone(), workspace.right.clone()] {
                    pane.update(cx, |pane, cx| pane.location_changed(None, false, cx));
                }
            }),
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
            _subscriptions: subscriptions,
        };
        workspace.apply_appearance(window, cx);
        workspace.reload_connections(true, cx);
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
        let changed = self.activate(side, cx);
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
            PaneEvent::EntriesDropped(dragged) => {
                let target = self.pane(side).read(cx).location().clone();
                // Dropping rows back where they came from does nothing.
                if dragged.source != target {
                    self.start_copy(plan(&dragged.source, &dragged.entries, &target), window, cx);
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
            PaneEvent::RowActionRequested(RowAction::Sync, source, entry) => {
                self.show_sync(side, source, Some(&entry.name), window, cx);
            }
            PaneEvent::RowActionRequested(action, source, entry) => {
                self.show_file(*action, source, entry, window, cx);
            }
            PaneEvent::SyncRequested(source) => self.show_sync(side, source, None, window, cx),
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
        let title: SharedString = t!(
            "copy.across_title",
            count = items.len(),
            target = target_name
        )
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

    fn run_copy(&mut self, planned: Plan, window: &mut Window, cx: &mut Context<Self>) {
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
        if action == RowAction::Properties {
            let view = cx.new(|cx| PropertiesView::new(object, cx));
            let title: SharedString = t!("properties.title", name = name).into_owned().into();
            let width = rems(36.).to_pixels(window.rem_size());
            window.open_dialog(cx, move |dialog, _, _| {
                dialog.title(title.clone()).width(width).child(view.clone())
            });
            return;
        }
        if action == RowAction::Versions {
            let view = cx.new(|cx| VersionsView::new(object, cx));
            let title: SharedString = t!("versions.title", name = name).into_owned().into();
            let width = rems(46.).to_pixels(window.rem_size());
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
                    let width = rems(36.).to_pixels(window.rem_size());
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
        let width = rems(46.).to_pixels(window.rem_size());
        let view = cx.new(|cx| ObjectView::new(file, &entry.name, entry.size, guarded, window, cx));
        let changed = source.clone();
        cx.subscribe(&view, move |workspace, _, event, cx| match event {
            ObjectViewEvent::Saved => {
                for pane in [workspace.left.clone(), workspace.right.clone()] {
                    pane.update(cx, |pane, cx| {
                        pane.location_changed(Some(&changed), false, cx)
                    });
                }
            }
        })
        .detach();
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
        let width = rems(40.).to_pixels(window.rem_size());
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
    pub fn show_about(window: &mut Window, cx: &mut App) {
        const SOURCE: &str = env!("CARGO_PKG_REPOSITORY");
        const UPSTREAM: &str = "https://github.com/rgcsekaraa/brows3";
        const LICENSE_TEXT: &str = "https://www.gnu.org/licenses/agpl-3.0.html";
        let width = rems(30.).to_pixels(window.rem_size());
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
        let width = rems(34.).to_pixels(window.rem_size());
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
                        pane.update(cx, |pane, cx| pane.leave_profile(&profile_id, &home(), cx));
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

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_location = self.pane(self.active).read(cx).location().clone();
        let connections = self.connections.read(cx).profiles().to_vec();
        let workspace = cx.weak_entity();
        let settings = self.settings;

        let this_computer = SidebarMenuItem::new(t!("location.home").into_owned())
            .icon(IconName::HardDrive)
            .active(matches!(active_location, Location::Local(_)))
            .on_click(cx.listener(|workspace, _, _, cx| workspace.open_in_active(home(), cx)));

        let connection_items = connections.into_iter().map(|profile| {
            let is_active = active_location.profile_id() == Some(profile.id.as_str());
            let open = profile.clone();
            let menu_profile = profile.clone();
            let menu_workspace = workspace.clone();
            SidebarMenuItem::new(profile.name.clone())
                .icon(IconName::Globe)
                .active(is_active)
                .on_click(cx.listener(move |workspace, _, _, cx| {
                    workspace.open_in_active(start_of(&open), cx);
                }))
                .suffix(move |_, _| {
                    let profile = menu_profile.clone();
                    let workspace = menu_workspace.clone();
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
                        })
                })
        });
        let connection_items: Vec<_> = connection_items.collect();
        let has_connections = !self.connections.read(cx).profiles().is_empty();

        let place_item = |location: &Location, favorite: bool, cx: &mut Context<Self>| {
            let connection = location
                .profile_id()
                .map(|id| self.connections.read(cx).name_of(id))
                .unwrap_or_default();
            let target = location.clone();
            let item = SidebarMenuItem::new(short_name(location, &connection))
                .icon(if favorite {
                    IconName::Star
                } else {
                    match location {
                        Location::Local(_) => IconName::Folder,
                        _ => IconName::Globe,
                    }
                })
                .active(*location == active_location)
                .on_click(cx.listener(move |workspace, _, _, cx| {
                    workspace.open_in_active(target.clone(), cx);
                }));
            let full: SharedString = full_name(location, &connection).into();
            let places = self.places.clone();
            let removed = location.clone();
            // The full place and removal are in the item's context menu, so
            // nothing inside the row competes with opening it.
            item.accessibility_label(full.clone())
                .context_menu(move |menu, _, _| {
                    let (places, removed) = (places.clone(), removed.clone());
                    let menu = menu.label(full.clone());
                    if favorite {
                        menu.separator().item(
                            PopupMenuItem::new(t!("places.remove_favorite").into_owned()).on_click(
                                move |_, _, cx| {
                                    let removed = removed.clone();
                                    places.update(cx, |places, cx| {
                                        places.toggle_favorite(&removed);
                                        cx.notify();
                                    });
                                },
                            ),
                        )
                    } else {
                        menu
                    }
                })
        };
        let (favorites, recent) = {
            let places = self.places.read(cx);
            (places.favorites.clone(), places.recent.clone())
        };
        let favorite_items: Vec<_> = favorites
            .iter()
            .map(|location| place_item(location, true, cx))
            .collect();
        // Favorites are already one click away; recent lists the others.
        let recent_items: Vec<_> = recent
            .iter()
            .filter(|location| !favorites.contains(location))
            .take(5)
            .map(|location| place_item(location, false, cx))
            .collect();

        Sidebar::new("sidebar")
            .header(div().px_2().py_1().font_semibold().child("supaS3"))
            .child(
                SidebarGroup::new(t!("sidebar.places").into_owned())
                    .child(SidebarMenu::new().child(this_computer)),
            )
            .when(!favorite_items.is_empty(), |this| {
                this.child(
                    SidebarGroup::new(t!("sidebar.favorites").into_owned())
                        .child(SidebarMenu::new().children(favorite_items)),
                )
            })
            .when(!recent_items.is_empty(), |this| {
                this.child(
                    SidebarGroup::new(t!("sidebar.recent").into_owned())
                        .child(SidebarMenu::new().children(recent_items)),
                )
            })
            .child(
                SidebarGroup::new(t!("sidebar.connections").into_owned())
                    .child(SidebarMenu::new().children(connection_items)),
            )
            .footer(
                v_flex()
                    .w_full()
                    .gap_2()
                    .when(!has_connections, |this| {
                        this.child(
                            div()
                                .px_2()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("sidebar.no_connections").into_owned()),
                        )
                    })
                    .child(
                        Button::new("add-connection")
                            .w_full()
                            .icon(IconName::Plus)
                            .label(t!("sidebar.add_connection").into_owned())
                            .on_click(cx.listener(|workspace, _, window, cx| {
                                workspace.show_form(None, window, cx);
                            })),
                    )
                    .child(
                        Button::new("preferences")
                            .w_full()
                            .ghost()
                            .icon(IconName::Settings)
                            .label(t!("sidebar.preferences").into_owned())
                            .dropdown_menu(move |menu, _, _| {
                                let pick = |workspace: &WeakEntity<Workspace>,
                                            change: fn(&mut Settings)| {
                                    let workspace = workspace.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = workspace.update(cx, |workspace, cx| {
                                            workspace.change_settings(change, window, cx);
                                        });
                                    }
                                };
                                menu.label(t!("preferences.language").into_owned())
                                    .item(
                                        PopupMenuItem::new(
                                            t!("preferences.follow_system").into_owned(),
                                        )
                                        .checked(settings.language == Language::System)
                                        .on_click(pick(&workspace, |s| {
                                            s.language = Language::System
                                        })),
                                    )
                                    .item(
                                        PopupMenuItem::new("English")
                                            .checked(settings.language == Language::English)
                                            .on_click(pick(&workspace, |s| {
                                                s.language = Language::English
                                            })),
                                    )
                                    .item(
                                        PopupMenuItem::new("简体中文")
                                            .checked(
                                                settings.language == Language::SimplifiedChinese,
                                            )
                                            .on_click(pick(&workspace, |s| {
                                                s.language = Language::SimplifiedChinese
                                            })),
                                    )
                                    .separator()
                                    .label(t!("preferences.appearance").into_owned())
                                    .item(
                                        PopupMenuItem::new(
                                            t!("preferences.follow_system").into_owned(),
                                        )
                                        .checked(settings.appearance == Appearance::System)
                                        .on_click(pick(&workspace, |s| {
                                            s.appearance = Appearance::System
                                        })),
                                    )
                                    .item(
                                        PopupMenuItem::new(t!("preferences.light").into_owned())
                                            .checked(settings.appearance == Appearance::Light)
                                            .on_click(pick(&workspace, |s| {
                                                s.appearance = Appearance::Light
                                            })),
                                    )
                                    .item(
                                        PopupMenuItem::new(t!("preferences.dark").into_owned())
                                            .checked(settings.appearance == Appearance::Dark)
                                            .on_click(pick(&workspace, |s| {
                                                s.appearance = Appearance::Dark
                                            })),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new(t!("about.menu").into_owned())
                                            .on_click(|_, window, cx| Self::show_about(window, cx)),
                                    )
                            }),
                    ),
            )
    }

    fn render_pane(&self, side: Side, cx: &mut Context<Self>) -> impl IntoElement {
        // The active pane is outlined; sidebar choices open there.
        let outline = if self.active == side {
            cx.theme().ring
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
        h_flex()
            .items_stretch()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
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
                    .child(
                        div().flex_1().min_h_0().child(
                            h_resizable("panes")
                                .child(
                                    resizable_panel()
                                        .size_range(min_pane..Pixels::MAX)
                                        .child(self.render_pane(Side::Left, cx)),
                                )
                                .child(
                                    resizable_panel()
                                        .size_range(min_pane..Pixels::MAX)
                                        .child(self.render_pane(Side::Right, cx)),
                                ),
                        ),
                    )
                    .child(self.task_center.clone()),
            )
    }
}
