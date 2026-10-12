// SPDX-License-Identifier: AGPL-3.0-or-later
//! The table that shows the contents of one tab.

use crate::listing::{format_modified, format_size, Entry, EntryKind};
use crate::location::Location;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{h_flex, ActiveTheme as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use std::collections::BTreeSet;
use std::rc::Rc;

/// What a row can ask its pane to do. Copy and delete carry the rows and the
/// location they were read from, captured when the user opened the menu: the
/// list may reload before the command is chosen, and a row number would then
/// point at something else.
#[derive(Debug, Clone)]
pub enum RowCommand {
    /// The table scrolled near the end and wants the next page.
    LoadMore,
    /// The set of marked rows changed.
    MarksChanged,
    /// Copy these rows to the other pane.
    Copy(DraggedEntries),
    /// Delete these rows.
    Delete(DraggedEntries),
    /// Something for one row alone, whatever else is marked.
    One(RowAction, Location, Entry),
}

/// What can be asked for a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// Show the file, for editing where that is possible.
    Open,
    /// Make links to the object.
    Share,
    /// Show the object's version history.
    Versions,
    /// Sync the local folder to the other pane.
    Sync,
}

/// Carries a row command to the owning pane.
type Commands = Rc<dyn Fn(RowCommand, &mut Window, &mut App)>;

/// Rows taken from a pane, with the location they were in.
#[derive(Debug, Clone)]
pub struct DraggedEntries {
    pub source: Location,
    pub entries: Rc<Vec<Entry>>,
}

/// What follows the pointer while rows are dragged.
struct DragLabel(SharedString);

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .text_sm()
            .child(self.0.clone())
    }
}

/// Rows and loading state for one tab's table. The pane that owns the tab
/// writes them; the table only reads.
pub struct EntriesDelegate {
    entries: Vec<Entry>,
    /// Rows marked for a batch action, by position.
    marked: BTreeSet<usize>,
    /// The marked rows, built once per change of marks rather than once per
    /// row that is drawn.
    marked_rows: std::cell::RefCell<Option<Rc<Vec<Entry>>>>,
    /// Where the rows are, for dragging them elsewhere.
    location: Location,
    loading: bool,
    has_more: bool,
    /// The window's rem size when the table was created. The table component
    /// sizes columns in pixels, so widths are derived from it to keep them in
    /// proportion to the text.
    rem_size: Pixels,
    commands: Commands,
}

const MARK: usize = 0;
const NAME: usize = 1;
const SIZE: usize = 2;

impl EntriesDelegate {
    pub fn new(
        rem_size: Pixels,
        location: Location,
        commands: impl Fn(RowCommand, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            entries: Vec::new(),
            marked: BTreeSet::new(),
            marked_rows: Default::default(),
            location,
            loading: false,
            has_more: false,
            rem_size,
            commands: Rc::new(commands),
        }
    }

    /// Removes every row and mark, when a location is loaded from the start.
    pub fn clear(&mut self, location: Location) {
        self.entries.clear();
        self.set_marks(BTreeSet::new());
        self.location = location;
    }

    /// Adds the rows of the page that just arrived.
    pub fn append(&mut self, entries: Vec<Entry>) {
        self.entries.extend(entries);
    }

    /// `loading`: a request is in flight. `load_more`: the table may ask for
    /// the next page on its own when it scrolls near the end.
    pub fn set_state(&mut self, loading: bool, load_more: bool) {
        self.loading = loading;
        self.has_more = load_more;
    }

    pub fn entry(&self, row: usize) -> Option<&Entry> {
        self.entries.get(row)
    }

    pub fn marked_count(&self) -> usize {
        self.marked.len()
    }

    fn set_marks(&mut self, marked: BTreeSet<usize>) {
        self.marked = marked;
        self.marked_rows.replace(None);
    }

    pub fn toggle_mark(&mut self, row: usize) {
        if row < self.entries.len() {
            let mut marked = std::mem::take(&mut self.marked);
            if !marked.remove(&row) {
                marked.insert(row);
            }
            self.set_marks(marked);
        }
    }

    /// The rows an action applies to: the marked rows when `row` is one of
    /// them or there is no `row`, otherwise `row` alone. So acting on an
    /// unmarked row never silently includes rows marked elsewhere.
    pub fn selection(&self, row: Option<usize>) -> Rc<Vec<Entry>> {
        match row {
            Some(row) if !self.marked.contains(&row) => {
                Rc::new(self.entries.get(row).cloned().into_iter().collect())
            }
            _ => self
                .marked_rows
                .borrow_mut()
                .get_or_insert_with(|| {
                    Rc::new(
                        self.marked
                            .iter()
                            .filter_map(|row| self.entries.get(*row).cloned())
                            .collect(),
                    )
                })
                .clone(),
        }
    }

    /// The rows an action on `row` applies to, with where they are.
    fn taken(&self, row: Option<usize>) -> DraggedEntries {
        DraggedEntries {
            source: self.location.clone(),
            entries: self.selection(row),
        }
    }

    fn all_marked(&self) -> bool {
        !self.entries.is_empty() && self.marked.len() == self.entries.len()
    }
}

impl TableDelegate for EntriesDelegate {
    fn columns_count(&self, _: &App) -> usize {
        4
    }

    fn rows_count(&self, _: &App) -> usize {
        self.entries.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            MARK => Column::new("mark", "")
                .width(self.rem_size * 2.25)
                .resizable(false)
                .movable(false),
            NAME => Column::new("name", t!("column.name").into_owned()).width(self.rem_size * 15.),
            SIZE => Column::new("size", t!("column.size").into_owned()).width(self.rem_size * 6.),
            _ => Column::new("modified", t!("column.modified").into_owned())
                .width(self.rem_size * 10.),
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if col_ix == MARK {
            let all = self.all_marked();
            return h_flex()
                .size_full()
                .justify_center()
                .child(
                    Checkbox::new("mark-all")
                        .checked(all)
                        .accessibility_label(t!("select.all").into_owned())
                        .on_change(cx.listener(|table, checked: &bool, window, cx| {
                            let delegate = table.delegate_mut();
                            delegate.set_marks(if *checked {
                                (0..delegate.entries.len()).collect()
                            } else {
                                BTreeSet::new()
                            });
                            let commands = delegate.commands.clone();
                            commands(RowCommand::MarksChanged, window, cx);
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        let name = self.column(col_ix, cx).name;
        h_flex()
            .size_full()
            .when(col_ix == SIZE, |this| this.justify_end())
            .child(name)
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let dragged = self.taken(Some(row_ix));
        let label: SharedString = match dragged.entries.as_slice() {
            [only] => only.name.clone().into(),
            many => t!("drag.items", count = many.len()).into_owned().into(),
        };
        div()
            .id(("row", row_ix))
            .on_drag(dragged, move |_, _, _, cx| {
                cx.new(|_| DragLabel(label.clone()))
            })
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        // What the menu acts on is fixed now, while the user is looking at it.
        let taken = self.taken(Some(row_ix));
        let (copy, delete) = (self.commands.clone(), self.commands.clone());
        let (copied, deleted) = (taken.clone(), taken);
        // Actions for the row under the pointer alone.
        let mut menu = menu;
        if let Some(entry) = self.entries.get(row_ix) {
            let remote = matches!(self.location, Location::Objects { .. });
            let local = matches!(self.location, Location::Local(_));
            let actions: &[(RowAction, &str)] = match entry.kind {
                EntryKind::File if remote => &[
                    (RowAction::Open, "action.open"),
                    (RowAction::Share, "action.share_ellipsis"),
                    (RowAction::Versions, "action.versions_ellipsis"),
                ],
                EntryKind::File => &[(RowAction::Open, "action.open")],
                EntryKind::Folder if local => &[(RowAction::Sync, "action.sync_ellipsis")],
                _ => &[],
            };
            for (action, label) in actions.iter().copied() {
                let commands = self.commands.clone();
                let (location, entry) = (self.location.clone(), entry.clone());
                menu = menu.item(PopupMenuItem::new(t!(label).into_owned()).on_click(
                    move |_, window, cx| {
                        let command = RowCommand::One(action, location.clone(), entry.clone());
                        commands(command, window, cx)
                    },
                ));
            }
            if !actions.is_empty() {
                menu = menu.separator();
            }
        }
        menu.item(
            PopupMenuItem::new(t!("action.copy_to_other").into_owned())
                .on_click(move |_, window, cx| copy(RowCommand::Copy(copied.clone()), window, cx)),
        )
        .separator()
        .item(
            PopupMenuItem::new(t!("action.delete_ellipsis").into_owned()).on_click(
                move |_, window, cx| delete(RowCommand::Delete(deleted.clone()), window, cx),
            ),
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(entry) = self.entries.get(row_ix) else {
            return div().into_any_element();
        };
        match col_ix {
            MARK => h_flex()
                .size_full()
                .justify_center()
                .child(
                    Checkbox::new(("mark", row_ix))
                        .checked(self.marked.contains(&row_ix))
                        .accessibility_label(
                            t!("select.item", name = entry.name.as_str()).into_owned(),
                        )
                        .on_change(cx.listener(move |table, _: &bool, window, cx| {
                            let delegate = table.delegate_mut();
                            delegate.toggle_mark(row_ix);
                            let commands = delegate.commands.clone();
                            commands(RowCommand::MarksChanged, window, cx);
                            cx.notify();
                        })),
                )
                .into_any_element(),
            NAME => {
                let icon = match entry.kind {
                    EntryKind::Bucket => IconName::HardDrive,
                    EntryKind::Folder => IconName::Folder,
                    EntryKind::File => IconName::File,
                };
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        Icon::new(icon)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().truncate().child(entry.name.clone()))
                    .into_any_element()
            }
            // Sizes are compared down the column, so they align on the right.
            SIZE => h_flex()
                .w_full()
                .justify_end()
                .text_color(cx.theme().muted_foreground)
                .child(entry.size.map(format_size).unwrap_or_default())
                .into_any_element(),
            _ => div()
                .text_color(cx.theme().muted_foreground)
                .child(
                    entry
                        .modified
                        .as_deref()
                        .map(format_modified)
                        .unwrap_or_default(),
                )
                .into_any_element(),
        }
    }

    fn loading(&self, _: &App) -> bool {
        // The first page replaces the table with a loading state; further
        // pages load while the rows stay visible.
        self.loading && self.entries.is_empty()
    }

    fn has_more(&self, _: &App) -> bool {
        self.has_more
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        (self.commands)(RowCommand::LoadMore, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::EntriesDelegate;
    use crate::listing::{Entry, EntryKind};
    use crate::location::Location;

    fn delegate(names: &[&str]) -> EntriesDelegate {
        let location = Location::Local(std::env::temp_dir());
        let mut delegate = EntriesDelegate::new(gpui_kit::px(16.), location, |_, _, _| {});
        delegate.append(
            names
                .iter()
                .map(|name| Entry {
                    name: name.to_string(),
                    kind: EntryKind::File,
                    size: None,
                    modified: None,
                    prefix: None,
                })
                .collect(),
        );
        delegate
    }

    fn names(entries: std::rc::Rc<Vec<Entry>>) -> Vec<String> {
        entries.iter().map(|entry| entry.name.clone()).collect()
    }

    #[test]
    fn an_action_on_a_marked_row_covers_all_marked_rows() {
        let mut delegate = delegate(&["a", "b", "c"]);
        delegate.toggle_mark(0);
        delegate.toggle_mark(2);
        assert_eq!(delegate.marked_count(), 2);
        assert_eq!(names(delegate.selection(Some(2))), ["a", "c"]);
        assert_eq!(names(delegate.selection(None)), ["a", "c"]);
    }

    #[test]
    fn an_action_on_an_unmarked_row_covers_only_that_row() {
        let mut delegate = delegate(&["a", "b", "c"]);
        delegate.toggle_mark(0);
        assert_eq!(names(delegate.selection(Some(1))), ["b"]);
        assert!(names(delegate.selection(Some(9))).is_empty());
    }

    #[test]
    fn marks_toggle_and_are_dropped_with_the_rows() {
        let mut delegate = delegate(&["a", "b"]);
        delegate.toggle_mark(1);
        delegate.toggle_mark(1);
        delegate.toggle_mark(7);
        assert_eq!(delegate.marked_count(), 0);
        assert!(names(delegate.selection(None)).is_empty());

        delegate.toggle_mark(0);
        delegate.clear(Location::Local(std::env::temp_dir()));
        assert_eq!(delegate.marked_count(), 0);
        assert!(delegate.entry(0).is_none());
    }

    #[test]
    fn rows_taken_for_a_command_do_not_change_when_the_list_does() {
        let mut delegate = delegate(&["a", "b", "c"]);
        delegate.toggle_mark(1);
        let taken = delegate.taken(Some(1));
        let single = delegate.taken(Some(2));

        // The list reloads with different content before the command runs.
        delegate.clear(Location::Local(std::env::temp_dir().join("elsewhere")));
        delegate.append(vec![]);

        assert_eq!(names(taken.entries.clone()), ["b"]);
        assert_eq!(names(single.entries), ["c"]);
        assert_eq!(taken.source, Location::Local(std::env::temp_dir()));
    }

    #[test]
    fn marks_keep_pointing_at_the_same_rows_when_a_page_is_appended() {
        let mut delegate = delegate(&["a", "b"]);
        delegate.toggle_mark(1);
        assert_eq!(names(delegate.selection(None)), ["b"]);
        delegate.append(vec![Entry {
            name: "c".into(),
            kind: EntryKind::File,
            size: None,
            modified: None,
            prefix: None,
        }]);
        delegate.toggle_mark(2);
        assert_eq!(names(delegate.selection(None)), ["b", "c"]);
    }
}
