// SPDX-License-Identifier: AGPL-3.0-or-later
//! Interaction tests of the workspace in a headless window, without a
//! storage backend: what the keyboard does needs none.

use super::{Side, Workspace};
use crate::app_jobs::AppJobs;
use crate::app_state::AppBackend;
use crate::settings::Settings;
use crate::tasks::EventQueue;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    px, size, AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext,
    WindowBounds, WindowOptions,
};
use std::sync::Arc;

fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    open_with(
        cx,
        Err(anyhow::anyhow!("no backend in interaction tests")),
        Arc::new(EventQueue::default()),
    )
}

fn open_with(
    cx: &mut TestAppContext,
    backend: anyhow::Result<crate::backend::Backend>,
    events: Arc<EventQueue>,
) -> (AnyWindowHandle, Entity<Workspace>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(AppBackend::new(backend));
        cx.set_global(AppJobs::default());
        crate::bind_keys(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1200.), px(760.)),
            })),
            ..Default::default()
        };
        let (window, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Workspace::new(Settings::default(), events, window, cx))
        })
        .expect("open test window");
        (window.into(), workspace)
    })
}

/// Where the keyboard is: 0 the sidebar, 1 the left pane, 2 the right one,
/// 3 the transfer list, 4 the details panel.
fn region(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> Option<usize> {
    cx.update_window(window, |_, window, cx| {
        let workspace = workspace.read(cx);
        if workspace.sidebar_focus.contains_focused(window, cx) {
            Some(0)
        } else if workspace.left.read(cx).contains_focus(window, cx) {
            Some(1)
        } else if workspace.right.read(cx).contains_focus(window, cx) {
            Some(2)
        } else if workspace
            .task_center
            .read(cx)
            .focus_handle()
            .contains_focused(window, cx)
        {
            Some(3)
        } else if workspace
            .details
            .read(cx)
            .focus_handle()
            .contains_focused(window, cx)
        {
            Some(4)
        } else {
            None
        }
    })
    .unwrap()
}

#[gpui_kit::test]
async fn tab_moves_between_the_sidebar_and_the_panes(cx: &mut TestAppContext) {
    let (window, workspace) = open(cx);
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
    // The active pane has the keyboard from the start.
    let start = match cx.update(|cx| workspace.read(cx).active) {
        Side::Left => 1,
        Side::Right => 2,
    };
    assert_eq!(region(cx, window, &workspace), Some(start));

    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
        region(cx, window, &workspace)
    };
    // Tab goes round: sidebar, left pane, right pane, transfer list.
    let forwards: Vec<_> = (1..=4).map(|step| Some((start + step) % 4)).collect();
    let seen: Vec<_> = (0..4).map(|_| press(cx, "tab")).collect();
    assert_eq!(seen, forwards);
    let backwards: Vec<_> = (1..=4).map(|step| Some((start + 4 - step) % 4)).collect();
    let seen: Vec<_> = (0..4).map(|_| press(cx, "shift-tab")).collect();
    assert_eq!(seen, backwards);
    // With the details panel open, it is on the way too, after the panes.
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.toggle_details(window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    let mut seen = Vec::new();
    while seen.last() != Some(&Some(2)) {
        assert!(seen.len() < 10, "never reached the right pane: {seen:?}");
        seen.push(press(cx, "tab"));
    }
    assert_eq!(press(cx, "tab"), Some(4));
    assert_eq!(press(cx, "tab"), Some(3));
    assert_eq!(press(cx, "shift-tab"), Some(4));
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.toggle_details(window, cx))
    })
    .unwrap();
    cx.run_until_parked();

    // Moving into a pane makes it the active one.
    let mut seen = Vec::new();
    while seen.last() != Some(&Some(1)) {
        assert!(seen.len() < 10, "never reached the left pane: {seen:?}");
        seen.push(press(cx, "tab"));
    }
    cx.update(|cx| assert_eq!(workspace.read(cx).active, Side::Left));
    assert_eq!(press(cx, "tab"), Some(2));
    cx.update(|cx| assert_eq!(workspace.read(cx).active, Side::Right));
}

/// Runs the app until `ready` holds, in real time: a backend answers from
/// its own threads.
fn until(cx: &mut TestAppContext, what: &str, mut ready: impl FnMut(&mut TestAppContext) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        if ready(cx) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Timers in the app run on the test's clock, not the wall's.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
    }
}

/// Everything that needs the storage backend is in this one test, which
/// runs alone among the tests that start one: the backend's runtime is
/// registered for the whole process.
#[gpui_kit::test]
async fn on_this_computer_filter_new_folder_and_move_work(cx: &mut TestAppContext) {
    use crate::listing::{Entry, EntryKind};
    use crate::location::Location;
    use gpui_kit::AppContext as _;

    cx.executor().allow_parking();
    let _serial = crate::backend::one_backend_at_a_time();
    let root = std::env::temp_dir().join(format!("supas3-ui-backend-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (left_dir, right_dir) = (root.join("left"), root.join("right"));
    std::fs::create_dir_all(&left_dir).unwrap();
    std::fs::create_dir_all(&right_dir).unwrap();
    for name in ["alpha.txt", "beta.txt", "notes.md"] {
        std::fs::write(left_dir.join(name), name).unwrap();
    }
    let events = Arc::new(EventQueue::default());
    let backend = crate::backend::Backend::start(root.join("config"), events.sink());
    let (window, workspace) = open_with(cx, backend, events);
    cx.update(|cx| {
        let workspace = workspace.read(cx);
        let (left, right) = (workspace.left.clone(), workspace.right.clone());
        left.update(cx, |pane, cx| {
            pane.open(Location::Local(left_dir.clone()), cx)
        });
        right.update(cx, |pane, cx| {
            pane.open(Location::Local(right_dir.clone()), cx)
        });
    });
    let shown = |cx: &mut TestAppContext, side: Side| {
        cx.update(|cx| {
            let pane = workspace.read(cx).pane(side).clone();
            let pane = pane.read(cx);
            pane.shown_count(cx)
        })
    };
    until(cx, "the left folder to list", |cx| {
        shown(cx, Side::Left) == (3, 3)
    });
    let dialog_open = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| {
            use gpui_kit::component::WindowExt as _;
            window.has_active_dialog(cx)
        })
        .unwrap()
    };

    // Shift+F10 opens the selected row's menu, as a list in a dialog.
    cx.update(|cx| workspace.update(cx, |workspace, _| workspace.active = Side::Left));
    cx.update_window(window, |_, window, cx| {
        let left = workspace.read(cx).left.clone();
        left.update(cx, |pane, cx| pane.focus(window, cx));
        window.render_frame(cx);
        window.press("shift-f10", cx);
    })
    .unwrap();
    until(cx, "the row menu", |cx| dialog_open(cx));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // Open, Rename, then the commands for everything selected.
        window.find(("row-menu", 0usize));
        window.press("escape", cx);
    })
    .unwrap();
    until(cx, "the row menu to close", |cx| !dialog_open(cx));

    // Filter: only the loaded rows whose names match stay.
    cx.update(|cx| workspace.update(cx, |workspace, _| workspace.active = Side::Left));
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.pane_command(super::PaneCommand::Filter, window, cx)
        });
        window.input("note", cx);
    })
    .unwrap();
    until(cx, "the filter", |cx| shown(cx, Side::Left) == (1, 3));

    // New folder, named in its dialog.
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.pane_command(super::PaneCommand::NewFolder, window, cx)
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.press("secondary-a", cx);
        window.input("made", cx);
        window.press("enter", cx);
    })
    .unwrap();
    until(cx, "the new folder", |_| left_dir.join("made").is_dir());
    until(cx, "the name dialog to close", |cx| !dialog_open(cx));

    // Move a file to the other pane: asked first, then done.
    let entry = Entry {
        name: "alpha.txt".into(),
        kind: EntryKind::File,
        size: Some(9),
        modified: None,
        prefix: None,
    };
    let source = Location::Local(left_dir.clone());
    let planned =
        crate::transfer_plan::plan(&source, &[entry], &Location::Local(right_dir.clone()));
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.start_move(planned, &source, window, cx)
        });
    })
    .unwrap();
    until(cx, "the move to be asked about", dialog_open);
    // The dialog slides in on the wall clock; a click during that may miss,
    // so it is made again until the dialog has gone.
    until(cx, "the move to be confirmed", |cx| {
        if dialog_open(cx) {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                if window.try_find("confirm-move").is_some() {
                    window.click("confirm-move", cx);
                }
            })
            .unwrap();
        }
        !dialog_open(cx)
    });
    until(cx, "the move", |_| {
        right_dir.join("alpha.txt").is_file() && !left_dir.join("alpha.txt").exists()
    });
    assert_eq!(
        std::fs::read_to_string(right_dir.join("alpha.txt")).unwrap(),
        "alpha.txt"
    );

    cx.update(|cx| AppBackend::shutdown(cx));
    let _ = std::fs::remove_dir_all(root);
}
