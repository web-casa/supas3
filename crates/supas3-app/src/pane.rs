// SPDX-License-Identifier: AGPL-3.0-or-later
//! One side of the workspace: a set of tabs, each showing a location.

use crate::app_state::{AppBackend, Connections};
use crate::entries_table::{DraggedEntries, EntriesDelegate, RowAction, RowCommand};
use crate::listing::Entry;
use crate::listing::{Listing, LoadState, Page};
use crate::location::{collapse, CrumbSlot, History, Location};
use gpui_kit::component::breadcrumb::{Breadcrumb, BreadcrumbItem};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// How many breadcrumb steps a pane shows before collapsing the middle.
const MAX_CRUMBS: usize = 3;

/// What a pane asks the workspace to do. Copying needs the other pane, and
/// deleting needs a confirmation, so neither is carried out here.
pub enum PaneEvent {
    /// The user started working in this pane.
    Activated,
    /// Copy these rows, read from the given location, to the other pane.
    CopyRequested(DraggedEntries),
    /// Delete these rows, read from the given location.
    DeleteRequested(DraggedEntries),
    /// Files were dragged in from outside the application.
    FilesDropped(Vec<std::path::PathBuf>),
    /// Rows were dragged in from a pane.
    EntriesDropped(DraggedEntries),
    /// Do something with this one row of the given location.
    RowActionRequested(RowAction, Location, Entry),
    /// Sync the local folder this pane shows to the other pane.
    SyncRequested(Location),
}

struct PaneTab {
    /// Stable for the tab's lifetime; keys its elements and async answers.
    id: u64,
    history: History,
    listing: Listing,
    /// Something changed what this location holds while the tab could not
    /// be reloaded; it is reloaded as soon as that is safe.
    stale: bool,
    table: Entity<TableState<EntriesDelegate>>,
    _table_events: Subscription,
}

pub struct Pane {
    /// Distinguishes the two panes' element ids.
    side: &'static str,
    connections: Entity<Connections>,
    tabs: Vec<PaneTab>,
    active: usize,
    next_tab_id: u64,
    path_input: Entity<InputState>,
    editing_path: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaneEvent> for Pane {}

impl Pane {
    pub fn new(
        side: &'static str,
        start: Location,
        connections: Entity<Connections>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path_input = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&path_input, window, Self::on_path_input),
            // Connection names appear in tabs and breadcrumbs.
            cx.observe(&connections, |_, _, cx| cx.notify()),
        ];
        let mut pane = Self {
            side,
            connections,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 0,
            path_input,
            editing_path: false,
            _subscriptions: subscriptions,
        };
        pane.add_tab(start, window, cx);
        pane
    }

    /// The location of the active tab.
    pub fn location(&self) -> &Location {
        self.tabs[self.active].history.current()
    }

    /// Opens a location in a new tab and shows it.
    pub fn add_tab(&mut self, location: Location, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let pane = cx.weak_entity();
        let delegate = EntriesDelegate::new(
            window.rem_size(),
            location.clone(),
            move |command, window, cx| {
                // Rows ask while the table is rendering or handling its own
                // event; answer once that has finished.
                let pane = pane.clone();
                window.defer(cx, move |_, cx| {
                    let _ = pane.update(cx, |pane, cx| pane.on_row_command(id, command, cx));
                });
            },
        );
        let table = cx.new(|cx| TableState::new(delegate, window, cx));
        let table_events =
            cx.subscribe_in(&table, window, move |pane, _, event: &TableEvent, _, cx| {
                if let TableEvent::DoubleClickedRow(row) = event {
                    pane.open_row(id, *row, cx);
                }
            });
        self.tabs.push(PaneTab {
            id,
            history: History::new(location),
            listing: Listing::default(),
            stale: false,
            table,
            _table_events: table_events,
        });
        self.active = self.tabs.len() - 1;
        self.editing_path = false;
        self.load(id, false, cx);
    }

    /// Closes a tab. The last tab stays: a pane always shows something.
    pub fn close_tab(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        let Some(index) = self.tab_index(id) else {
            return;
        };
        self.tabs.remove(index);
        if self.active > index || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.editing_path = false;
        cx.notify();
    }

    /// Shows a location in the active tab.
    pub fn open(&mut self, location: Location, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active];
        tab.history.visit(location);
        let id = tab.id;
        self.editing_path = false;
        self.load(id, false, cx);
    }

    /// Whether the pane still shows only what it started with: one tab that
    /// has not been navigated.
    pub fn is_untouched(&self) -> bool {
        self.tabs.len() == 1 && self.tabs[0].history.is_single()
    }

    /// Removes a deleted connection from every tab: tabs showing it go to
    /// `fallback`, and no tab can go Back or Forward into it.
    pub fn leave_profile(&mut self, profile_id: &str, fallback: &Location, cx: &mut Context<Self>) {
        let mut reload = Vec::new();
        for tab in &mut self.tabs {
            let was_showing = tab.history.current().profile_id() == Some(profile_id);
            tab.history.forget_profile(profile_id, fallback);
            if was_showing {
                reload.push(tab.id);
            }
        }
        for id in reload {
            self.load(id, false, cx);
        }
        cx.notify();
    }

    /// Reloads every tab that shows an edited connection. Rows and paging
    /// tokens fetched with the old settings must not be mixed with the new.
    pub fn reload_profile(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let affected: Vec<u64> = self
            .tabs
            .iter()
            .filter(|tab| tab.history.current().profile_id() == Some(profile_id))
            .map(|tab| tab.id)
            .collect();
        for id in affected {
            self.load(id, true, cx);
        }
    }

    /// Re-reads translated labels the tables keep, after a language change.
    pub fn refresh_labels(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            tab.table.update(cx, |table, cx| table.refresh(cx));
        }
        cx.notify();
    }

    /// An element id that is unique across both panes.
    fn id(&self, name: &str) -> ElementId {
        SharedString::from(format!("{}-{name}", self.side)).into()
    }

    fn on_row_command(&mut self, tab_id: u64, command: RowCommand, cx: &mut Context<Self>) {
        match command {
            RowCommand::LoadMore => self.load_more(tab_id, cx),
            RowCommand::MarksChanged => {
                self.reload_stale(cx);
                cx.notify();
            }
            RowCommand::Copy(rows) => cx.emit(PaneEvent::CopyRequested(rows)),
            RowCommand::Delete(rows) => cx.emit(PaneEvent::DeleteRequested(rows)),
            RowCommand::One(action, location, entry) => {
                cx.emit(PaneEvent::RowActionRequested(action, location, entry));
            }
        }
    }

    /// The rows a toolbar or keyboard action applies to, with where they are:
    /// the marked rows, or the selected row when nothing is marked.
    fn selection(&self, cx: &App) -> DraggedEntries {
        let tab = &self.tabs[self.active];
        let table = tab.table.read(cx);
        let delegate = table.delegate();
        DraggedEntries {
            source: tab.history.current().clone(),
            entries: if delegate.marked_count() > 0 {
                delegate.selection(None)
            } else {
                delegate.selection(table.selected_row())
            },
        }
    }

    /// Notes that what a location holds has changed, and reloads the tabs
    /// showing it. `location` of `None` means any location may have changed.
    /// A tab that is loading or has marked rows is reloaded later, when that
    /// no longer disturbs the user; `now` reloads it regardless, for when the
    /// user's own action has made its rows and marks out of date.
    pub fn location_changed(
        &mut self,
        location: Option<&Location>,
        now: bool,
        cx: &mut Context<Self>,
    ) {
        for tab in &mut self.tabs {
            if location.is_none_or(|changed| tab.history.current() == changed) {
                tab.stale = true;
            }
        }
        if now {
            let tab = &self.tabs[self.active];
            if tab.stale {
                let id = tab.id;
                self.load(id, true, cx);
                return;
            }
        }
        self.reload_stale(cx);
    }

    /// Reloads the active tab if it is stale and reloading is safe.
    fn reload_stale(&mut self, cx: &mut Context<Self>) {
        let tab = &self.tabs[self.active];
        let marked = tab.table.read(cx).delegate().marked_count();
        if tab.stale && !tab.listing.is_busy() && marked == 0 {
            let id = tab.id;
            self.load(id, true, cx);
        }
    }

    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active];
        if tab.history.go_back() {
            let id = tab.id;
            self.load(id, false, cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active];
        if tab.history.go_forward() {
            let id = tab.id;
            self.load(id, false, cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.location().parent() {
            self.open(parent, cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let id = self.tabs[self.active].id;
        self.load(id, true, cx);
    }

    fn open_row(&mut self, tab_id: u64, row: usize, cx: &mut Context<Self>) {
        let Some(index) = self.tab_index(tab_id) else {
            return;
        };
        let tab = &self.tabs[index];
        let current = tab.history.current();
        let Some(entry) = tab.table.read(cx).delegate().entry(row).cloned() else {
            return;
        };
        if !entry.opens_a_location() {
            // A file: show it.
            let event = PaneEvent::RowActionRequested(RowAction::Open, current.clone(), entry);
            cx.emit(event);
            return;
        }
        let target = match (&entry.prefix, current) {
            // A remote folder opens at the exact prefix the service gave.
            (
                Some(prefix),
                Location::Objects {
                    profile_id, bucket, ..
                },
            ) => Location::objects(profile_id, bucket, prefix),
            _ => current.child(&entry.name),
        };
        self.active = index;
        self.open(target, cx);
    }

    /// Loads a tab's current location from the start.
    fn load(&mut self, tab_id: u64, fresh: bool, cx: &mut Context<Self>) {
        let Some(index) = self.tab_index(tab_id) else {
            return;
        };
        let generation = self.tabs[index].listing.begin();
        self.tabs[index].stale = false;
        let location = self.tabs[index].history.current().clone();
        self.tabs[index].table.update(cx, |table, cx| {
            // The rows and their selection belong to what is being replaced.
            table.delegate_mut().clear(location);
            table.clear_selection(cx);
        });
        self.sync_state(index, cx);
        self.request(tab_id, generation, None, fresh, cx);
        cx.notify();
    }

    fn load_more(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.tab_index(tab_id) else {
            return;
        };
        if let Some((generation, token)) = self.tabs[index].listing.begin_more() {
            self.sync_state(index, cx);
            self.request(tab_id, generation, Some(token), false, cx);
            cx.notify();
        }
    }

    fn request(
        &mut self,
        tab_id: u64,
        generation: u64,
        token: Option<String>,
        fresh: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index(tab_id) else {
            return;
        };
        let location = self.tabs[index].history.current().clone();
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            self.finish(tab_id, generation, Err(message), cx);
            return;
        };
        cx.spawn(async move |pane, cx| {
            let result = requests.list(location, token, fresh).await;
            let _ = pane.update(cx, |pane, cx| pane.finish(tab_id, generation, result, cx));
        })
        .detach();
    }

    fn finish(
        &mut self,
        tab_id: u64,
        generation: u64,
        result: Result<Page, String>,
        cx: &mut Context<Self>,
    ) {
        // The tab may have been closed while the request was running.
        let Some(index) = self.tab_index(tab_id) else {
            return;
        };
        let Some(rows) = self.tabs[index].listing.finish(generation, result) else {
            return;
        };
        self.tabs[index]
            .table
            .update(cx, |table, _| table.delegate_mut().append(rows));
        self.sync_state(index, cx);
        // A change that arrived while this was loading is picked up now.
        self.reload_stale(cx);
        cx.notify();
    }

    /// Tells a tab's table whether it is loading and whether it may ask for
    /// more rows. Rows are handed over once, when they arrive; they are not
    /// copied again, and column widths the user adjusted are left alone.
    fn sync_state(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = &self.tabs[index];
        let loading = tab.listing.is_busy();
        let load_more = tab.listing.may_load_more_automatically();
        tab.table.update(cx, |table, cx| {
            table.delegate_mut().set_state(loading, load_more);
            cx.notify();
        });
    }

    /// Keyboard navigation for the pane. Keys reach here only when the focused
    /// control (the table, a button) did not use them itself.
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaneEvent::Activated);
        let keystroke = &event.keystroke;
        let plain = !keystroke.modifiers.modified();
        match keystroke.key.as_str() {
            "escape" if self.editing_path => {
                self.editing_path = false;
                cx.notify();
            }
            _ if self.editing_path => return,
            "enter" if plain => {
                let tab = &self.tabs[self.active];
                if let Some(row) = tab.table.read(cx).selected_row() {
                    self.open_row(tab.id, row, cx);
                }
            }
            "backspace" if plain => self.go_up(cx),
            "space" if plain => {
                let table = self.tabs[self.active].table.clone();
                table.update(cx, |table, cx| {
                    if let Some(row) = table.selected_row() {
                        table.delegate_mut().toggle_mark(row);
                        cx.notify();
                    }
                });
                cx.notify();
            }
            "delete" if plain => {
                let rows = self.selection(cx);
                if !rows.entries.is_empty() {
                    cx.emit(PaneEvent::DeleteRequested(rows));
                }
            }
            "left" if keystroke.modifiers.alt => self.go_back(cx),
            "right" if keystroke.modifiers.alt => self.go_forward(cx),
            "up" if keystroke.modifiers.alt => self.go_up(cx),
            "f5" => self.refresh(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn on_path_input(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => {
                let text = input.read(cx).value();
                match self.location().resolve_path_text(&text) {
                    Some(location) => self.open(location, cx),
                    None => {
                        self.editing_path = false;
                        cx.notify();
                    }
                }
            }
            InputEvent::Blur => {
                self.editing_path = false;
                cx.notify();
            }
            _ => {}
        }
    }

    fn edit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.location().path_text();
        self.path_input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
        self.editing_path = true;
        cx.notify();
    }

    /// What a tab is called: the last step of its location.
    fn tab_label(&self, location: &Location, cx: &App) -> SharedString {
        match location {
            Location::Buckets { profile_id } => {
                self.connections.read(cx).name_of(profile_id).into()
            }
            Location::Local(path) if path.parent().is_none() => {
                t!("location.this_computer").into_owned().into()
            }
            _ => location
                .crumbs()
                .last()
                .map(|crumb| crumb.label.clone())
                .unwrap_or_default()
                .into(),
        }
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let closable = self.tabs.len() > 1;
        TabBar::new(self.id("tabs"))
            .w_full()
            .selected_index(self.active)
            .on_click(cx.listener(|pane, index: &usize, _, cx| {
                pane.active = (*index).min(pane.tabs.len() - 1);
                pane.editing_path = false;
                pane.reload_stale(cx);
                cx.emit(PaneEvent::Activated);
                cx.notify();
            }))
            .children(self.tabs.iter().map(|tab| {
                let id = tab.id;
                let label = self.tab_label(tab.history.current(), cx);
                Tab::new().label(label).when(closable, |this| {
                    this.suffix(
                        Button::new(self.id(&format!("close-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t!("tab.close").into_owned())
                            .on_click(cx.listener(move |pane, _, _, cx| {
                                cx.stop_propagation();
                                pane.close_tab(id, cx);
                            })),
                    )
                })
            }))
            .suffix(
                h_flex().px_1().child(
                    Button::new(self.id("new-tab"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip(t!("tab.new").into_owned())
                        .on_click(cx.listener(|pane, _, window, cx| {
                            let location = pane.location().clone();
                            pane.add_tab(location, window, cx);
                            cx.emit(PaneEvent::Activated);
                        })),
                ),
            )
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = &self.tabs[self.active];
        let location = tab.history.current();
        let nav_button = |name: &'static str, icon: IconName, tooltip: String| {
            Button::new(self.id(name))
                .ghost()
                .small()
                .icon(icon)
                .tooltip(tooltip)
        };

        let path = if self.editing_path {
            Input::new(&self.path_input).small().into_any_element()
        } else {
            let current = location.clone();
            Breadcrumb::new()
                .children(
                    collapse(location.crumbs(), MAX_CRUMBS)
                        .into_iter()
                        .map(|slot| {
                            let crumb = match slot {
                                CrumbSlot::Step(crumb) => crumb,
                                CrumbSlot::Gap => return BreadcrumbItem::new("…").disabled(true),
                            };
                            let label: SharedString = match &crumb.location {
                                Location::Buckets { profile_id } => {
                                    self.connections.read(cx).name_of(profile_id).into()
                                }
                                _ => crumb.label.clone().into(),
                            };
                            let item = BreadcrumbItem::new(label);
                            if crumb.location == current {
                                item
                            } else {
                                let target = crumb.location.clone();
                                item.on_click(cx.listener(move |pane, _, _, cx| {
                                    pane.open(target.clone(), cx);
                                }))
                            }
                        }),
                )
                .into_any_element()
        };

        h_flex()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                nav_button("back", IconName::ArrowLeft, t!("nav.back").into_owned())
                    .disabled(!tab.history.can_go_back())
                    .on_click(cx.listener(|pane, _, _, cx| pane.go_back(cx))),
            )
            .child(
                nav_button(
                    "forward",
                    IconName::ArrowRight,
                    t!("nav.forward").into_owned(),
                )
                .disabled(!tab.history.can_go_forward())
                .on_click(cx.listener(|pane, _, _, cx| pane.go_forward(cx))),
            )
            .child(
                nav_button("up", IconName::ArrowUp, t!("nav.up").into_owned())
                    .disabled(location.parent().is_none())
                    .on_click(cx.listener(|pane, _, _, cx| pane.go_up(cx))),
            )
            .child(
                nav_button(
                    "refresh",
                    IconName::RotateCw,
                    t!("nav.refresh").into_owned(),
                )
                .disabled(tab.listing.is_busy())
                .on_click(cx.listener(|pane, _, _, cx| pane.refresh(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_1()
                    .overflow_hidden()
                    .child(path),
            )
            .when(!self.editing_path, |this| {
                this.child(
                    Button::new(self.id("edit-path"))
                        .ghost()
                        .small()
                        .label(t!("nav.go_to").into_owned())
                        .on_click(cx.listener(|pane, _, window, cx| pane.edit_path(window, cx))),
                )
            })
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = &self.tabs[self.active];
        let empty = tab.listing.count() == 0;
        match tab.listing.state() {
            LoadState::Failed(message) if empty => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .large()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(div().child(t!("list.failed").into_owned()))
                .child(
                    div()
                        .max_w(rems(36.))
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(message.clone()),
                )
                .child(
                    Button::new(self.id("retry"))
                        .label(t!("action.retry").into_owned())
                        .on_click(cx.listener(|pane, _, _, cx| pane.refresh(cx))),
                )
                .into_any_element(),
            LoadState::Loaded if empty => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::Inbox).large())
                .child(t!("list.empty").into_owned())
                .into_any_element(),
            _ => DataTable::new(&tab.table)
                .stripe(false)
                .bordered(false)
                .into_any_element(),
        }
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = &self.tabs[self.active];
        let count = tab.listing.count();
        let (text, failed) = match tab.listing.state() {
            LoadState::Idle | LoadState::Loading => (t!("status.loading").into_owned(), false),
            LoadState::LoadingMore => {
                (t!("status.loading_more", count = count).into_owned(), false)
            }
            LoadState::Failed(message) if count > 0 => (
                t!("status.more_failed", count = count, error = message).into_owned(),
                true,
            ),
            LoadState::Failed(_) => (String::new(), false),
            LoadState::Loaded if tab.listing.has_more() => {
                (t!("status.items_more", count = count).into_owned(), false)
            }
            LoadState::Loaded => (t!("status.items", count = count).into_owned(), false),
        };
        let can_load_more = tab.listing.has_more() && !tab.listing.is_busy();
        let marked = tab.table.read(cx).delegate().marked_count();
        let has_selection = !self.selection(cx).entries.is_empty();
        let text = if marked > 0 {
            t!("status.marked", marked = marked, count = count).into_owned()
        } else {
            text
        };
        let is_local = matches!(tab.history.current(), Location::Local(_));
        // The arrow points at the other pane.
        let copy_icon = if self.side == "left" {
            IconName::ArrowRight
        } else {
            IconName::ArrowLeft
        };
        h_flex()
            .w_full()
            .h_7()
            .px_3()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(if failed {
                cx.theme().danger
            } else {
                cx.theme().muted_foreground
            })
            .child(div().flex_1().min_w_0().truncate().child(text))
            .when(can_load_more, |this| {
                let id = tab.id;
                this.child(
                    Button::new(self.id("load-more"))
                        .ghost()
                        .xsmall()
                        .label(t!("action.load_more").into_owned())
                        .on_click(cx.listener(move |pane, _, _, cx| pane.load_more(id, cx))),
                )
            })
            .when(is_local, |this| {
                this.child(
                    Button::new(self.id("sync"))
                        .ghost()
                        .xsmall()
                        .label(t!("action.sync_ellipsis").into_owned())
                        .tooltip(t!("sync.button_hint").into_owned())
                        .on_click(cx.listener(|pane, _, _, cx| {
                            let location = pane.location().clone();
                            cx.emit(PaneEvent::SyncRequested(location));
                        })),
                )
            })
            .child(
                Button::new(self.id("copy"))
                    .xsmall()
                    .icon(copy_icon)
                    .label(t!("action.copy_to_other").into_owned())
                    .disabled(!has_selection)
                    .on_click(cx.listener(|pane, _, _, cx| {
                        let rows = pane.selection(cx);
                        cx.emit(PaneEvent::CopyRequested(rows));
                    })),
            )
            .child(
                Button::new(self.id("delete"))
                    .ghost()
                    .xsmall()
                    .label(t!("action.delete_ellipsis").into_owned())
                    .disabled(!has_selection)
                    .on_click(cx.listener(|pane, _, _, cx| {
                        let rows = pane.selection(cx);
                        cx.emit(PaneEvent::DeleteRequested(rows));
                    })),
            )
    }
}

impl Render for Pane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Activated)),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            // Dropping onto a pane copies into the location it shows.
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .drag_over::<DraggedEntries>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(|_, paths: &ExternalPaths, _, cx| {
                cx.emit(PaneEvent::Activated);
                cx.emit(PaneEvent::FilesDropped(paths.paths().to_vec()));
            }))
            .on_drop(cx.listener(|_, dragged: &DraggedEntries, _, cx| {
                cx.emit(PaneEvent::Activated);
                cx.emit(PaneEvent::EntriesDropped(dragged.clone()));
            }))
            .child(self.render_tabs(cx))
            .child(self.render_navigation(cx))
            .child(div().flex_1().min_h_0().child(self.render_content(cx)))
            .child(self.render_status(cx))
    }
}
