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
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(AppBackend::new(Err(anyhow::anyhow!(
            "no backend in interaction tests"
        ))));
        cx.set_global(AppJobs::default());
        crate::bind_keys(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1200.), px(760.)),
            })),
            ..Default::default()
        };
        let events = Arc::new(EventQueue::default());
        let (window, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Workspace::new(Settings::default(), events, window, cx))
        })
        .expect("open test window");
        (window.into(), workspace)
    })
}

/// Where the keyboard is: 0 the sidebar, 1 the left pane, 2 the right one.
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
    // Tab goes round: sidebar, left pane, right pane.
    let forwards: Vec<_> = (1..=3).map(|step| Some((start + step) % 3)).collect();
    let seen: Vec<_> = (0..3).map(|_| press(cx, "tab")).collect();
    assert_eq!(seen, forwards);
    let backwards: Vec<_> = (1..=3).map(|step| Some((start + 3 - step) % 3)).collect();
    let seen: Vec<_> = (0..3).map(|_| press(cx, "shift-tab")).collect();
    assert_eq!(seen, backwards);

    // Moving into a pane makes it the active one.
    while press(cx, "tab") != Some(1) {}
    cx.update(|cx| assert_eq!(workspace.read(cx).active, Side::Left));
    assert_eq!(press(cx, "tab"), Some(2));
    cx.update(|cx| assert_eq!(workspace.read(cx).active, Side::Right));
}
