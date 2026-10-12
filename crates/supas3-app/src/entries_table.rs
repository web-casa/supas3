// SPDX-License-Identifier: AGPL-3.0-or-later
//! The table that shows the contents of one tab.

use crate::listing::{format_modified, format_size, Entry, EntryKind};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{h_flex, ActiveTheme as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

/// Asks the owning pane for the next page.
type LoadMore = Box<dyn Fn(&mut Window, &mut App)>;

/// Rows and loading state for one tab's table. The pane that owns the tab
/// writes them; the table only reads.
pub struct EntriesDelegate {
    entries: Vec<Entry>,
    loading: bool,
    has_more: bool,
    /// The window's rem size when the table was created. The table component
    /// sizes columns in pixels, so widths are derived from it to keep them in
    /// proportion to the text.
    rem_size: Pixels,
    /// Called when the table scrolls near the end and more rows exist.
    on_load_more: LoadMore,
}

const NAME: usize = 0;
const SIZE: usize = 1;

impl EntriesDelegate {
    pub fn new(rem_size: Pixels, on_load_more: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Self {
            rem_size,
            entries: Vec::new(),
            loading: false,
            has_more: false,
            on_load_more: Box::new(on_load_more),
        }
    }

    /// Removes every row, when a location is loaded from the start.
    pub fn clear(&mut self) {
        self.entries.clear();
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
}

impl TableDelegate for EntriesDelegate {
    fn columns_count(&self, _: &App) -> usize {
        3
    }

    fn rows_count(&self, _: &App) -> usize {
        self.entries.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            NAME => Column::new("name", t!("column.name").into_owned()).width(self.rem_size * 16.),
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
        let name = self.column(col_ix, cx).name;
        h_flex()
            .size_full()
            .when(col_ix == SIZE, |this| this.justify_end())
            .child(name)
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
        (self.on_load_more)(window, cx);
    }
}
