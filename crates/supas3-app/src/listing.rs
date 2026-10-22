// SPDX-License-Identifier: AGPL-3.0-or-later
//! The contents of a location as a pane shows them, loaded a page at a time.

use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Bucket,
    Folder,
    File,
}

/// One row: a bucket, a folder (or common prefix), or a file (or object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name within the current location, without a trailing slash.
    pub name: String,
    pub kind: EntryKind,
    /// Size in bytes; `None` for buckets and folders.
    pub size: Option<u64>,
    /// Last modification time as the source reported it.
    pub modified: Option<String>,
    /// For a remote folder, the exact prefix the service reported. Object keys
    /// may contain empty segments (`a//b`), so the prefix to open is kept as
    /// given rather than rebuilt from the displayed name.
    pub prefix: Option<String>,
}

impl Entry {
    pub fn opens_a_location(&self) -> bool {
        matches!(self.kind, EntryKind::Bucket | EntryKind::Folder)
    }
}

/// One page of results from a source.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// Passed back to fetch the next page; `None` when there are no more.
    pub next_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// Nothing has been requested yet.
    Idle,
    /// The first page is on its way.
    Loading,
    /// Further pages are on their way; the loaded rows stay visible.
    LoadingMore,
    Loaded,
    /// The last request failed with this message. Rows loaded earlier stay.
    Failed(String),
}

/// What a tab knows about loading its current location. The rows themselves
/// live in the tab's table; this tracks how many there are and what comes
/// next.
///
/// Every request carries the generation it was started in. Navigating starts
/// a new generation, so an answer that arrives late for a location the user
/// has already left is recognised and dropped instead of being shown.
#[derive(Debug, Clone)]
pub struct Listing {
    count: usize,
    next_token: Option<String>,
    state: LoadState,
    generation: u64,
}

impl Default for Listing {
    fn default() -> Self {
        Self {
            count: 0,
            next_token: None,
            state: LoadState::Idle,
            generation: 0,
        }
    }
}

impl Listing {
    /// The generation of the latest request.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// How many rows have been loaded.
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn state(&self) -> &LoadState {
        &self.state
    }

    pub fn has_more(&self) -> bool {
        self.next_token.is_some()
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.state, LoadState::Loading | LoadState::LoadingMore)
    }

    /// Whether the next page may be fetched without the user asking: only
    /// after a successful page. After a failure the user retries explicitly,
    /// so a failing service is not asked again on every repaint.
    pub fn may_load_more_automatically(&self) -> bool {
        self.has_more() && self.state == LoadState::Loaded
    }

    /// Starts loading a location from the beginning; the caller clears its
    /// rows. Returns the generation to hand back with the answer.
    pub fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.count = 0;
        self.next_token = None;
        self.state = LoadState::Loading;
        self.generation
    }

    /// Starts loading the next page, if there is one and nothing is in
    /// flight. Returns the generation and the token to request.
    pub fn begin_more(&mut self) -> Option<(u64, String)> {
        if self.is_busy() {
            return None;
        }
        let token = self.next_token.clone()?;
        self.state = LoadState::LoadingMore;
        Some((self.generation, token))
    }

    /// Records the answer to a request. Returns the rows to append when it
    /// was applied; `None` for a failure or for an answer from an earlier
    /// generation, which is ignored entirely.
    pub fn finish(&mut self, generation: u64, result: Result<Page, String>) -> Option<Vec<Entry>> {
        if generation != self.generation {
            return None;
        }
        match result {
            Ok(page) => {
                self.count += page.entries.len();
                self.next_token = page.next_token.filter(|token| !token.is_empty());
                self.state = LoadState::Loaded;
                Some(page.entries)
            }
            Err(message) => {
                self.state = LoadState::Failed(message);
                Some(Vec::new())
            }
        }
    }
}

/// Folders and buckets first, then names without regard to case, so the order
/// matches what file managers show.
pub fn display_order(left: &Entry, right: &Entry) -> Ordering {
    right
        .opens_a_location()
        .cmp(&left.opens_a_location())
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.name.cmp(&right.name))
}

/// The column a tab is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    #[default]
    Name,
    Size,
    Modified,
}

/// How a tab's rows are ordered. Folders always come first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Sort {
    pub field: SortField,
    pub descending: bool,
}

impl Sort {
    /// What to ask the storage backend for. `None` is its ordinary order,
    /// name ascending, read a page at a time; anything else makes it read
    /// the whole folder first.
    pub fn backend(self) -> Option<(&'static str, &'static str)> {
        let direction = if self.descending { "desc" } else { "asc" };
        match (self.field, self.descending) {
            (SortField::Name, false) => None,
            (SortField::Name, true) => Some(("name", direction)),
            (SortField::Size, _) => Some(("size", direction)),
            (SortField::Modified, _) => Some(("date", direction)),
        }
    }
}

/// Orders rows that are all loaded at once (a folder on this computer).
pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    entries.sort_by(|left, right| {
        let by_field = match sort.field {
            SortField::Name => display_order(left, right),
            SortField::Size => left
                .size
                .cmp(&right.size)
                .then_with(|| display_order(left, right)),
            SortField::Modified => left
                .modified
                .cmp(&right.modified)
                .then_with(|| display_order(left, right)),
        };
        let by_field = if sort.descending {
            by_field.reverse()
        } else {
            by_field
        };
        right
            .opens_a_location()
            .cmp(&left.opens_a_location())
            .then(by_field)
    });
}

/// Whether a row's name contains the filter text, ignoring case. An empty
/// filter keeps every row.
pub fn matches_filter(entry: &Entry, filter: &str) -> bool {
    filter.is_empty() || entry.name.to_lowercase().contains(&filter.to_lowercase())
}

/// Whether a page request failed because the backend no longer holds the
/// listing it was paging through; the tab then starts over.
pub fn is_expired_listing(error: &str) -> bool {
    error.contains("This listing has changed")
}

/// Whether a sorted listing was refused because the folder is too large to
/// read whole; the tab then falls back to name order.
pub fn is_too_large_to_sort(error: &str) -> bool {
    error.contains("safety limit for complete-result sorting")
}

/// A size as people read it: binary units, one decimal below ten.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Shortens an ISO 8601 timestamp to `YYYY-MM-DD HH:MM` for a table cell.
/// Anything else is shown as it is.
pub fn format_modified(timestamp: &str) -> String {
    let bytes = timestamp.as_bytes();
    let looks_iso = bytes.len() >= 16 && bytes[4] == b'-' && bytes[7] == b'-' && bytes[13] == b':';
    if looks_iso && (bytes[10] == b'T' || bytes[10] == b' ') {
        format!("{} {}", &timestamp[..10], &timestamp[11..16])
    } else {
        timestamp.to_string()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn folders_stay_first_whatever_the_order() {
        use super::{sort_entries, Entry, EntryKind, Sort, SortField};
        let row = |name: &str, kind, size| Entry {
            name: name.into(),
            kind,
            size,
            modified: None,
            prefix: None,
        };
        let mut rows = vec![
            row("b.txt", EntryKind::File, Some(5)),
            row("z", EntryKind::Folder, None),
            row("a.txt", EntryKind::File, Some(9)),
            row("y", EntryKind::Folder, None),
        ];
        sort_entries(
            &mut rows,
            Sort {
                field: SortField::Size,
                descending: true,
            },
        );
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["z", "y", "a.txt", "b.txt"]);
        sort_entries(&mut rows, Sort::default());
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["y", "z", "a.txt", "b.txt"]);
    }

    #[test]
    fn only_name_ascending_pages_without_reading_everything() {
        use super::{Sort, SortField};
        assert_eq!(Sort::default().backend(), None);
        let by = |field, descending| Sort { field, descending }.backend();
        assert_eq!(by(SortField::Name, true), Some(("name", "desc")));
        assert_eq!(by(SortField::Modified, false), Some(("date", "asc")));
        assert_eq!(by(SortField::Size, true), Some(("size", "desc")));
    }

    use super::{
        display_order, format_modified, format_size, Entry, EntryKind, Listing, LoadState, Page,
    };

    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(1),
            modified: None,
            prefix: None,
        }
    }

    fn page(names: &[&str], next: Option<&str>) -> Page {
        Page {
            entries: names.iter().map(|name| file(name)).collect(),
            next_token: next.map(str::to_string),
        }
    }

    fn names(rows: Option<Vec<Entry>>) -> Vec<String> {
        rows.unwrap().into_iter().map(|entry| entry.name).collect()
    }

    #[test]
    fn pages_accumulate_until_the_source_has_no_more() {
        let mut listing = Listing::default();
        let generation = listing.begin();
        assert_eq!(*listing.state(), LoadState::Loading);
        assert_eq!(
            names(listing.finish(generation, Ok(page(&["a", "b"], Some("t1"))))),
            ["a", "b"]
        );
        assert!(listing.has_more());

        let (generation, token) = listing.begin_more().unwrap();
        assert_eq!(token, "t1");
        assert_eq!(*listing.state(), LoadState::LoadingMore);
        assert!(listing.begin_more().is_none(), "one request at a time");
        assert_eq!(
            names(listing.finish(generation, Ok(page(&["c"], None)))),
            ["c"]
        );

        assert_eq!(listing.count(), 3);
        assert!(!listing.has_more());
        assert!(listing.begin_more().is_none());
    }

    #[test]
    fn an_answer_for_a_location_the_user_left_is_dropped() {
        let mut listing = Listing::default();
        let first = listing.begin();
        let second = listing.begin();

        assert!(listing.finish(first, Ok(page(&["stale"], None))).is_none());
        assert_eq!(listing.count(), 0);
        assert_eq!(*listing.state(), LoadState::Loading);

        assert_eq!(
            names(listing.finish(second, Ok(page(&["fresh"], None)))),
            ["fresh"]
        );
    }

    #[test]
    fn a_page_requested_before_a_refresh_is_dropped() {
        let mut listing = Listing::default();
        let generation = listing.begin();
        listing.finish(generation, Ok(page(&["a"], Some("t1"))));
        let (old_generation, _) = listing.begin_more().unwrap();

        // The user refreshes while the next page is still on its way.
        let refreshed = listing.begin();
        assert!(listing
            .finish(old_generation, Ok(page(&["late"], Some("t2"))))
            .is_none());
        assert_eq!(listing.count(), 0);
        assert!(!listing.has_more(), "the stale token must not be kept");

        assert_eq!(
            names(listing.finish(refreshed, Ok(page(&["a"], None)))),
            ["a"]
        );
    }

    #[test]
    fn a_failed_page_keeps_the_count_and_waits_for_an_explicit_retry() {
        let mut listing = Listing::default();
        let generation = listing.begin();
        listing.finish(generation, Ok(page(&["a"], Some("t1"))));
        assert!(listing.may_load_more_automatically());

        let (generation, _) = listing.begin_more().unwrap();
        assert!(names(listing.finish(generation, Err("timeout".into()))).is_empty());

        assert_eq!(*listing.state(), LoadState::Failed("timeout".into()));
        assert_eq!(listing.count(), 1);
        assert!(listing.has_more());
        assert!(
            !listing.may_load_more_automatically(),
            "a failing service must not be asked again on every repaint"
        );
        // The user can still ask for the same page again.
        assert_eq!(
            listing.begin_more().map(|(_, token)| token).as_deref(),
            Some("t1")
        );
    }

    #[test]
    fn an_empty_token_means_no_more_pages() {
        let mut listing = Listing::default();
        let generation = listing.begin();
        listing.finish(generation, Ok(page(&["a"], Some(""))));
        assert!(!listing.has_more());
    }

    #[test]
    fn folders_sort_before_files_and_names_ignore_case() {
        let folder = |name: &str| Entry {
            kind: EntryKind::Folder,
            size: None,
            ..file(name)
        };
        let mut entries = [file("b.txt"), folder("Zoo"), file("A.txt"), folder("apple")];
        entries.sort_by(display_order);
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["apple", "Zoo", "A.txt", "b.txt"]);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(10 * 1024 * 1024), "10 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn timestamps_are_shortened_when_they_are_iso_dates() {
        assert_eq!(format_modified("2026-10-12T08:30:15Z"), "2026-10-12 08:30");
        assert_eq!(format_modified("2026-10-12 08:30:15"), "2026-10-12 08:30");
        assert_eq!(format_modified("yesterday"), "yesterday");
        assert_eq!(format_modified(""), "");
    }
}
