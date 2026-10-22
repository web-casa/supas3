// SPDX-License-Identifier: AGPL-3.0-or-later
//! One side of the workspace: a set of tabs, each showing a location.

use crate::app_state::{AppBackend, Connections};
use crate::entries_table::{DraggedEntries, EntriesDelegate, RowAction, RowCommand};
use crate::error_details::ErrorDetails;
use crate::listing::Entry;
use crate::listing::{is_expired_listing, is_too_large_to_sort, Listing, LoadState, Page, Sort};
use crate::location::{collapse, CrumbSlot, History, Location};
use crate::places::{is_memorable, PaneLayout, Places};
use gpui_kit::component::breadcrumb::{Breadcrumb, BreadcrumbItem};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// Whether folders can be created here: on this computer or in a bucket.
pub fn can_hold_folders(location: &Location) -> bool {
    matches!(location, Location::Local(_) | Location::Objects { .. })
}

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
    /// Put these rows' addresses on the clipboard, or their plain web
    /// addresses when `public`.
    AddressesRequested { rows: DraggedEntries, public: bool },
    /// Files were dragged in from outside the application.
    FilesDropped(Vec<std::path::PathBuf>),
    /// Rows were dragged in from a pane; `true` when they are to be moved.
    EntriesDropped(DraggedEntries, bool),
    /// Move these rows, read from the given location, to the other pane.
    MoveRequested(DraggedEntries),
    /// Do something with this one row of the given location.
    RowActionRequested(RowAction, Location, Entry),
    /// Sync the local folder this pane shows to the other pane.
    SyncRequested(Location),
    /// Import web addresses into this location of a bucket.
    ImportRequested(Location),
    /// The tabs or what they show changed; the layout is worth saving.
    Moved,
    /// Create a folder in the location shown.
    NewFolderRequested(Location),
    /// Something the user should know that is not an error of this pane.
    Notice(String),
    /// Search for names containing this text under the location, at any
    /// depth.
    SearchRequested(Location, String),
    /// The selected or marked rows, or the rows loaded, changed.
    SelectionChanged,
}

struct PaneTab {
    /// Stable for the tab's lifetime; keys its elements and async answers.
    id: u64,
    history: History,
    listing: Listing,
    /// Something changed what this location holds while the tab could not
    /// be reloaded; it is reloaded as soon as that is safe.
    stale: bool,
    /// How the rows are ordered; kept when the tab moves elsewhere.
    sort: Sort,
    /// Keep loading pages until the location is complete.
    load_all: bool,
    table: Entity<TableState<EntriesDelegate>>,
    _table_events: Subscription,
}

/// A file open for reading or editing in a tab of its own.
struct EditorTab {
    id: u64,
    name: String,
    view: Entity<crate::object_view::ObjectView>,
    _subscriptions: Vec<Subscription>,
}

pub struct Pane {
    /// Distinguishes the two panes' element ids.
    side: &'static str,
    /// Files open in tabs after the location tabs. They are not part of the
    /// saved layout.
    editors: Vec<EditorTab>,
    /// The editor tab in front, if one is; otherwise the location tab
    /// `active` is.
    front_editor: Option<u64>,
    connections: Entity<Connections>,
    places: Entity<Places>,
    tabs: Vec<PaneTab>,
    active: usize,
    next_tab_id: u64,
    path_input: Entity<InputState>,
    editing_path: bool,
    filter_input: Entity<InputState>,
    /// The filter bar is shown.
    filtering: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaneEvent> for Pane {}

impl Pane {
    /// Opens the tabs of `layout`, which must have at least one.
    pub fn new(
        side: &'static str,
        layout: PaneLayout,
        connections: Entity<Connections>,
        places: Entity<Places>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path_input = cx.new(|cx| InputState::new(window, cx));
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("filter.placeholder").into_owned())
        });
        let subscriptions = vec![
            cx.subscribe_in(&path_input, window, Self::on_path_input),
            cx.subscribe_in(&filter_input, window, Self::on_filter_input),
            // Connection names appear in tabs and breadcrumbs.
            cx.observe(&connections, |_, _, cx| cx.notify()),
            // The favorite mark follows changes made anywhere.
            cx.observe(&places, |_, _, cx| cx.notify()),
        ];
        let mut pane = Self {
            side,
            editors: Vec::new(),
            front_editor: None,
            connections,
            places,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 0,
            path_input,
            editing_path: false,
            filter_input,
            filtering: false,
            _subscriptions: subscriptions,
        };
        for (location, sort) in layout.tabs.into_iter().zip(layout.sorts) {
            pane.add_sorted_tab(location, sort, window, cx);
        }
        pane.active = layout.active.min(pane.tabs.len().saturating_sub(1));
        pane
    }

    /// Orders a tab's rows anew: reloads it from the start in that order.
    fn set_sort(&mut self, tab_id: u64, sort: Sort, cx: &mut Context<Self>) {
        let Some(index) = self.tab_index(tab_id) else {
            return;
        };
        self.tabs[index].sort = sort;
        self.tabs[index].table.update(cx, |table, cx| {
            table.delegate_mut().set_sort(sort);
            // The column headers show the order.
            table.refresh(cx);
        });
        self.load(tab_id, false, cx);
    }

    /// The names of the rows loaded in the active tab.
    pub fn loaded_names(&self, cx: &App) -> Vec<String> {
        self.tabs[self.active].table.read(cx).delegate().all_names()
    }

    /// Follows a folder that was renamed: tabs inside it show the same
    /// place under its new name.
    pub fn relocate(&mut self, from: &Location, to: &Location, cx: &mut Context<Self>) {
        let moved: Vec<(u64, Location)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                crate::places::relocated(tab.history.current(), from, to).map(|new| (tab.id, new))
            })
            .collect();
        for (id, new) in moved {
            if let Some(index) = self.tab_index(id) {
                self.tabs[index].history.visit(new);
                self.load(id, false, cx);
            }
        }
    }

    /// The tabs as they would be restored.
    pub fn layout(&self) -> PaneLayout {
        PaneLayout {
            tabs: self
                .tabs
                .iter()
                .map(|tab| tab.history.current().clone())
                .collect(),
            active: self.active,
            sorts: self.tabs.iter().map(|tab| tab.sort).collect(),
        }
    }

    /// The location of the active tab.
    pub fn location(&self) -> &Location {
        self.tabs[self.active].history.current()
    }

    /// Opens a location in a new tab and shows it.
    pub fn add_tab(&mut self, location: Location, window: &mut Window, cx: &mut Context<Self>) {
        let sort = self
            .tabs
            .get(self.active)
            .map(|tab| tab.sort)
            .unwrap_or_default();
        self.add_sorted_tab(location, sort, window, cx);
    }

    fn add_sorted_tab(
        &mut self,
        location: Location,
        sort: Sort,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let mut delegate = delegate;
        delegate.set_sort(sort);
        let table = cx.new(|cx| TableState::new(delegate, window, cx));
        let table_events = cx.subscribe_in(
            &table,
            window,
            move |pane, _, event: &TableEvent, _, cx| match event {
                TableEvent::DoubleClickedRow(row) => pane.open_row(id, *row, cx),
                TableEvent::SelectRow(_) | TableEvent::ClearSelection => {
                    cx.emit(PaneEvent::SelectionChanged)
                }
                _ => {}
            },
        );
        self.tabs.push(PaneTab {
            id,
            history: History::new(location),
            listing: Listing::default(),
            stale: false,
            sort,
            load_all: false,
            table,
            _table_events: table_events,
        });
        self.active = self.tabs.len() - 1;
        self.front_editor = None;
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
        cx.emit(PaneEvent::Moved);
        cx.emit(PaneEvent::SelectionChanged);
        cx.notify();
    }

    /// Shows a location in the active tab.
    pub fn open(&mut self, location: Location, cx: &mut Context<Self>) {
        // A place opened from elsewhere is shown, not hidden behind a file.
        self.front_editor = None;
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
            RowCommand::Sort(sort) => self.set_sort(tab_id, sort, cx),
            RowCommand::MarksChanged => {
                self.reload_stale(cx);
                cx.emit(PaneEvent::SelectionChanged);
                cx.notify();
            }
            RowCommand::Copy(rows) => cx.emit(PaneEvent::CopyRequested(rows)),
            RowCommand::Move(rows) => cx.emit(PaneEvent::MoveRequested(rows)),
            RowCommand::Delete(rows) => cx.emit(PaneEvent::DeleteRequested(rows)),
            RowCommand::CopyAddresses { rows, public } => {
                cx.emit(PaneEvent::AddressesRequested { rows, public })
            }
            RowCommand::One(action, location, entry) => {
                cx.emit(PaneEvent::RowActionRequested(action, location, entry));
            }
        }
    }

    /// The rows a toolbar or keyboard action applies to, with where they are:
    /// the marked rows, or the selected row when nothing is marked.
    /// Whether the location's rows are what the keyboard acts on: not while
    /// a file is in front or the path is being typed.
    fn rows_in_front(&self) -> bool {
        self.front_editor.is_none() && !self.editing_path
    }

    /// Asks to rename the one selected row.
    pub fn rename_selected(&mut self, cx: &mut Context<Self>) {
        if !self.rows_in_front() || matches!(self.location(), Location::Buckets { .. }) {
            return;
        }
        let rows = self.selection(cx);
        if let [entry] = rows.entries.as_slice() {
            cx.emit(PaneEvent::RowActionRequested(
                RowAction::Rename,
                rows.source.clone(),
                entry.clone(),
            ));
        }
    }

    /// Asks for a new folder in the location shown.
    pub fn new_folder(&mut self, cx: &mut Context<Self>) {
        if !self.rows_in_front() {
            return;
        }
        let location = self.location().clone();
        if can_hold_folders(&location) {
            cx.emit(PaneEvent::NewFolderRequested(location));
        }
    }

    /// Puts keyboard focus on the table in front, on its selected row or
    /// the first one.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self
            .front_editor
            .and_then(|id| self.editors.iter().find(|tab| tab.id == id))
        {
            let view = tab.view.clone();
            view.update(cx, |view, cx| view.focus(window, cx));
            return;
        }
        self.editing_path = false;
        let table = self.tabs[self.active].table.clone();
        table.update(cx, |table, cx| {
            if table.selected_row().is_none() && table.delegate().shown_count().0 > 0 {
                table.set_selected_row(0, cx);
            }
        });
        let handle = table.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Whether keyboard focus is anywhere in this pane.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.editors
            .iter()
            .any(|tab| tab.view.read(cx).contains_focus(window, cx))
            || self
                .path_input
                .focus_handle(cx)
                .contains_focused(window, cx)
            || self
                .filter_input
                .focus_handle(cx)
                .contains_focused(window, cx)
            || self
                .tabs
                .iter()
                .any(|tab| tab.table.focus_handle(cx).contains_focused(window, cx))
    }

    /// Opens a file in a new tab in front.
    pub fn open_editor(
        &mut self,
        name: String,
        view: Entity<crate::object_view::ObjectView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::object_view::ObjectViewEvent;
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let subscriptions = vec![
            // The tab shows whether there are unsaved edits.
            cx.observe(&view, |_, _, cx| cx.notify()),
            cx.subscribe_in(&view, window, move |pane, _, event, window, cx| {
                if let ObjectViewEvent::CloseRequested = event {
                    pane.remove_editor(id, window, cx);
                }
            }),
        ];
        self.editors.push(EditorTab {
            id,
            name,
            view: view.clone(),
            _subscriptions: subscriptions,
        });
        self.front_editor = Some(id);
        // The text takes the keyboard once it has loaded.
        window.defer(cx, move |window, cx| {
            view.update(cx, |view, cx| view.focus(window, cx));
        });
        cx.emit(PaneEvent::Activated);
        cx.notify();
    }

    /// Asks the editor tab to close; it asks the user first if edits would
    /// be lost.
    fn close_editor(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.editors.iter().find(|tab| tab.id == id) else {
            return;
        };
        let view = tab.view.clone();
        if view.read(cx).holds_work(cx) {
            // The question is asked in the tab; bring it to the front.
            self.front_editor = Some(id);
        }
        view.update(cx, |view, cx| view.close(window, cx));
        cx.notify();
    }

    fn remove_editor(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.editors.retain(|tab| tab.id != id);
        if self.front_editor == Some(id) {
            self.front_editor = self.editors.last().map(|tab| tab.id);
            if self.front_editor.is_none() {
                self.focus(window, cx);
            }
        }
        cx.notify();
    }

    /// Every file open in a tab here, with its name.
    pub fn editors(&self) -> Vec<(String, Entity<crate::object_view::ObjectView>)> {
        self.editors
            .iter()
            .map(|tab| (tab.name.clone(), tab.view.clone()))
            .collect()
    }

    /// Closes the tabs of files that `close` picks, unasked: the caller has
    /// made sure nothing is lost.
    pub fn drop_editors(
        &mut self,
        close: impl Fn(&crate::object_view::ObjectView, &App) -> bool,
        cx: &mut Context<Self>,
    ) {
        self.editors.retain(|tab| !close(tab.view.read(cx), cx));
        if self
            .front_editor
            .is_some_and(|id| !self.editors.iter().any(|tab| tab.id == id))
        {
            self.front_editor = None;
        }
        cx.notify();
    }

    /// What the details panel describes for this pane: the selected rows,
    /// or the location when none is selected.
    pub fn details_subject(&self, cx: &App) -> (Location, crate::details::Subject) {
        // A file in front is what the pane shows.
        if let Some(tab) = self
            .front_editor
            .and_then(|id| self.editors.iter().find(|tab| tab.id == id))
        {
            use crate::object_actions::FileRef;
            let entry = Entry {
                name: tab.name.clone(),
                kind: crate::listing::EntryKind::File,
                size: None,
                modified: None,
                prefix: None,
            };
            match tab.view.read(cx).file().clone() {
                FileRef::Remote(object) => {
                    let parent = object
                        .key
                        .rsplit_once('/')
                        .map(|(parent, _)| format!("{parent}/"))
                        .unwrap_or_default();
                    let source = Location::objects(&object.profile_id, &object.bucket, &parent);
                    return (source, crate::details::Subject::Object { object, entry });
                }
                FileRef::Local(path) => {
                    let source = Location::Local(
                        path.parent()
                            .map(|parent| parent.to_path_buf())
                            .unwrap_or_default(),
                    );
                    return (source, crate::details::Subject::LocalFile { path, entry });
                }
            }
        }
        let rows = self.selection(cx);
        let loaded = self.tabs[self.active]
            .table
            .read(cx)
            .delegate()
            .shown_count()
            .1;
        let subject = crate::details::subject(&rows.source, &rows.entries, loaded);
        (rows.source, subject)
    }

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
        if !fresh {
            cx.emit(PaneEvent::Moved);
        }
        let Some(index) = self.tab_index(tab_id) else {
            return;
        };
        let location = self.tabs[index].history.current().clone();
        // A new location starts unfiltered, and the table drops the filter
        // too; reloading the same one keeps it.
        let moved = self.tabs[index].table.read(cx).delegate().location() != &location;
        if moved && index == self.active {
            self.filtering = false;
        }
        // Loading every page was asked for the location as it was.
        self.tabs[index].load_all = false;
        let generation = self.tabs[index].listing.begin();
        self.tabs[index].stale = false;
        self.tabs[index].table.update(cx, |table, cx| {
            // The rows and their selection belong to what is being replaced.
            table.delegate_mut().clear(location);
            table.clear_selection(cx);
        });
        self.sync_state(index, cx);
        self.request(tab_id, generation, None, fresh, cx);
        cx.emit(PaneEvent::SelectionChanged);
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
        let sort = self.tabs[index].sort;
        let Some(requests) = AppBackend::requests(cx) else {
            let message = t!("error.backend_stopped").into_owned();
            self.finish(tab_id, generation, Err(message), cx);
            return;
        };
        cx.spawn(async move |pane, cx| {
            let result = requests.list(location, token, fresh, sort).await;
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
        if let Err(error) = &result {
            let current = self.tabs[index].listing.generation() == generation;
            // The backend dropped the sorted listing being paged through:
            // start over rather than stop with an error.
            if current
                && *self.tabs[index].listing.state() == LoadState::LoadingMore
                && is_expired_listing(error)
            {
                self.load(tab_id, true, cx);
                return;
            }
            // Too large to read whole: fall back to name order and say so.
            if current && is_too_large_to_sort(error) && self.tabs[index].sort != Sort::default() {
                cx.emit(PaneEvent::Notice(t!("sort.too_large").into_owned()));
                self.set_sort(tab_id, Sort::default(), cx);
                return;
            }
        }
        if result.is_err() {
            self.tabs[index].load_all = false;
        }
        let Some(rows) = self.tabs[index].listing.finish(generation, result) else {
            return;
        };
        self.tabs[index]
            .table
            .update(cx, |table, _| table.delegate_mut().append(rows));
        self.sync_state(index, cx);
        cx.emit(PaneEvent::SelectionChanged);
        if self.tabs[index].load_all {
            if self.tabs[index].listing.has_more() {
                self.load_more(tab_id, cx);
            } else {
                self.tabs[index].load_all = false;
            }
        }
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
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaneEvent::Activated);
        if self.front_editor.is_some() {
            return;
        }
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
                cx.emit(PaneEvent::SelectionChanged);
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
            "f" if keystroke.modifiers.secondary() && !keystroke.modifiers.shift => {
                self.open_filter(window, cx);
            }
            "escape" if self.filtering => self.close_filter(window, cx),
            "f2" if plain => self.rename_selected(cx),
            "n" if keystroke.modifiers.secondary() && keystroke.modifiers.shift => {
                self.new_folder(cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn on_filter_input(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::Change = event {
            let text = input.read(cx).value().to_string();
            self.apply_filter(&text, cx);
        }
        let _ = window;
    }

    fn apply_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        let table = self.tabs[self.active].table.clone();
        table.update(cx, |table, cx| {
            table.delegate_mut().set_filter(text);
            table.clear_selection(cx);
            cx.notify();
        });
        cx.emit(PaneEvent::SelectionChanged);
        cx.notify();
    }

    /// How many rows the tab in front shows, out of how many it loaded.
    #[cfg(test)]
    pub fn shown_count(&self, cx: &App) -> (usize, usize) {
        self.tabs[self.active]
            .table
            .read(cx)
            .delegate()
            .shown_count()
    }

    /// Shows the filter bar and puts the cursor in it.
    pub fn open_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.rows_in_front() {
            return;
        }
        self.filtering = true;
        // Show the filter the tab in front has (none after navigating).
        let filter = self.tabs[self.active]
            .table
            .read(cx)
            .delegate()
            .filter()
            .to_string();
        let input = self.filter_input.clone();
        input.update(cx, |input, cx| {
            input.set_value(filter, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// Hides the filter bar and shows every row again.
    fn close_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filtering = false;
        self.filter_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.apply_filter("", cx);
    }

    /// Loads every remaining page, so the filter covers the whole location.
    fn load_all(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tab_index(tab_id) {
            self.tabs[index].load_all = true;
            self.load_more(tab_id, cx);
        }
    }

    fn render_filter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = &self.tabs[self.active];
        let text = self.filter_input.read(cx).value().to_string();
        let remote = matches!(tab.history.current(), Location::Objects { .. });
        let more = tab.listing.has_more();
        let id = tab.id;
        h_flex()
            .w_full()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w(rems(14.))
                    .flex_shrink_0()
                    .child(Input::new(&self.filter_input).small()),
            )
            .child(div().flex_1())
            .when(more && !tab.listing.is_busy(), |this| {
                this.child(
                    Button::new(self.id("load-all"))
                        .ghost()
                        .xsmall()
                        .label(t!("filter.load_all").into_owned())
                        .on_click(cx.listener(move |pane, _, _, cx| pane.load_all(id, cx))),
                )
            })
            .when(remote && !text.trim().is_empty(), |this| {
                let location = tab.history.current().clone();
                let query = text.trim().to_string();
                this.child(
                    Button::new(self.id("search"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Search)
                        .label(t!("filter.search_below").into_owned())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(PaneEvent::SearchRequested(location.clone(), query.clone()));
                        })),
                )
            })
            .child(
                Button::new(self.id("close-filter"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(t!("filter.close").into_owned())
                    .on_click(cx.listener(|pane, _, window, cx| pane.close_filter(window, cx))),
            )
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
        let selected = match self
            .front_editor
            .and_then(|id| self.editors.iter().position(|tab| tab.id == id))
        {
            Some(position) => self.tabs.len() + position,
            None => self.active,
        };
        let editor_tabs: Vec<Tab> = self
            .editors
            .iter()
            .map(|tab| {
                let id = tab.id;
                let view = tab.view.read(cx);
                let label = if view.holds_work(cx) {
                    format!("{} •", tab.name)
                } else {
                    tab.name.clone()
                };
                Tab::new()
                    .prefix(Icon::new(IconName::File).xsmall())
                    .label(label)
                    .suffix(
                        Button::new(self.id(&format!("close-editor-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t!("tab.close").into_owned())
                            .on_click(cx.listener(move |pane, _, window, cx| {
                                cx.stop_propagation();
                                pane.close_editor(id, window, cx);
                            })),
                    )
            })
            .collect();
        TabBar::new(self.id("tabs"))
            .w_full()
            .selected_index(selected)
            .on_click(cx.listener(|pane, index: &usize, window, cx| {
                if let Some(tab) = index
                    .checked_sub(pane.tabs.len())
                    .and_then(|position| pane.editors.get(position))
                {
                    pane.front_editor = Some(tab.id);
                    let view = tab.view.clone();
                    view.update(cx, |view, cx| view.focus(window, cx));
                    cx.emit(PaneEvent::Activated);
                    cx.emit(PaneEvent::SelectionChanged);
                    cx.notify();
                    return;
                }
                pane.front_editor = None;
                pane.active = (*index).min(pane.tabs.len() - 1);
                pane.editing_path = false;
                // The bar shows the filter of the tab now in front.
                let filter = pane.tabs[pane.active]
                    .table
                    .read(cx)
                    .delegate()
                    .filter()
                    .to_string();
                pane.filtering = !filter.is_empty();
                pane.filter_input
                    .update(cx, |input, cx| input.set_value(filter, window, cx));
                pane.reload_stale(cx);
                cx.emit(PaneEvent::Activated);
                cx.emit(PaneEvent::Moved);
                cx.emit(PaneEvent::SelectionChanged);
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
            .children(editor_tabs)
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
                    nav_button("filter", IconName::Search, t!("filter.open").into_owned())
                        .on_click(cx.listener(|pane, _, window, cx| pane.open_filter(window, cx))),
                )
            })
            .when(!self.editing_path && is_memorable(location), |this| {
                let favorite = self.places.read(cx).is_favorite(location);
                let target = location.clone();
                this.child(
                    nav_button(
                        "favorite",
                        if favorite {
                            IconName::StarFill
                        } else {
                            IconName::Star
                        },
                        if favorite {
                            t!("places.remove_favorite").into_owned()
                        } else {
                            t!("places.add_favorite").into_owned()
                        },
                    )
                    .on_click(cx.listener(move |pane, _, _, cx| {
                        let target = target.clone();
                        pane.places.update(cx, |places, cx| {
                            places.toggle_favorite(&target);
                            cx.notify();
                        });
                    })),
                )
            })
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
        // Without rows the table is not drawn; what is drawn instead keeps
        // its focus, so the keyboard still works in the pane.
        let focus = tab.table.focus_handle(cx);
        match tab.listing.state() {
            LoadState::Failed(message) if empty => v_flex()
                .track_focus(&focus)
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
                        .child(ErrorDetails::new(self.id("load-error"), message.clone())),
                )
                .child(
                    Button::new(self.id("retry"))
                        .label(t!("action.retry").into_owned())
                        .on_click(cx.listener(|pane, _, _, cx| pane.refresh(cx))),
                )
                .into_any_element(),
            LoadState::Loaded if empty => v_flex()
                .track_focus(&focus)
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
                t!(
                    "status.more_failed",
                    count = count,
                    error = crate::error_details::summary(message)
                )
                .into_owned(),
                true,
            ),
            LoadState::Failed(_) => (String::new(), false),
            LoadState::Loaded if tab.listing.has_more() => {
                (t!("status.items_more", count = count).into_owned(), false)
            }
            LoadState::Loaded => (t!("status.items", count = count).into_owned(), false),
        };
        let can_load_more = tab.listing.has_more() && !tab.listing.is_busy();
        // A later page failed: the rows stay, and the error is one click away.
        let more_failure = match tab.listing.state() {
            LoadState::Failed(message) if count > 0 => Some(message.clone()),
            _ => None,
        };
        let marked = tab.table.read(cx).delegate().marked_count();
        let has_selection = !self.selection(cx).entries.is_empty();
        let (shown, loaded) = tab.table.read(cx).delegate().shown_count();
        let filtered = !tab.table.read(cx).delegate().filter().is_empty();
        let text = if marked > 0 {
            t!("status.marked", marked = marked, count = count).into_owned()
        } else if filtered && tab.listing.has_more() {
            t!("filter.partial", shown = shown, loaded = loaded).into_owned()
        } else if filtered {
            t!("filter.complete", shown = shown, loaded = loaded).into_owned()
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
            .when_some(more_failure, |this, error| {
                this.child(
                    Button::new(self.id("failure-details"))
                        .ghost()
                        .xsmall()
                        .label(t!("error.show_details").into_owned())
                        .on_click(move |_, window, cx| {
                            let error = error.clone();
                            window.open_dialog(cx, move |dialog, _, _| {
                                dialog
                                    .title(t!("status.more_failed_title").into_owned())
                                    .child(ErrorDetails::new("more-failed", error.clone()))
                            });
                        }),
                )
            })
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
            .when(can_hold_folders(tab.history.current()), |this| {
                this.child(
                    Button::new(self.id("new-folder"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::FolderOpen)
                        .tooltip(t!("action.new_folder_ellipsis").into_owned())
                        .on_click(cx.listener(|pane, _, _, cx| {
                            let location = pane.location().clone();
                            cx.emit(PaneEvent::NewFolderRequested(location));
                        })),
                )
            })
            .when(
                matches!(tab.history.current(), Location::Objects { .. }),
                |this| {
                    this.child(
                        Button::new(self.id("import"))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Globe)
                            .tooltip(t!("import.button").into_owned())
                            .on_click(cx.listener(|pane, _, _, cx| {
                                let location = pane.location().clone();
                                cx.emit(PaneEvent::ImportRequested(location));
                            })),
                    )
                },
            )
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
            .on_drop(cx.listener(|_, dragged: &DraggedEntries, window, cx| {
                let moving = crate::entries_table::drag_moves(&window.modifiers());
                cx.emit(PaneEvent::Activated);
                cx.emit(PaneEvent::EntriesDropped(dragged.clone(), moving));
            }))
            .child(self.render_tabs(cx))
            .map(|this| {
                let front = self
                    .front_editor
                    .and_then(|id| self.editors.iter().find(|tab| tab.id == id));
                match front {
                    Some(tab) => this.child(div().flex_1().min_h_0().child(tab.view.clone())),
                    None => this
                        .child(self.render_navigation(cx))
                        .when(self.filtering, |this| this.child(self.render_filter(cx)))
                        .child(div().flex_1().min_h_0().child(self.render_content(cx)))
                        .child(self.render_status(cx)),
                }
            })
    }
}
