// SPDX-License-Identifier: AGPL-3.0-or-later
//! supaS3 desktop application.

// Release builds on Windows are GUI programs and must not open a console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_jobs;
mod app_state;
mod aws_profiles;
mod backend;
mod conflicts;
mod connection;
mod connection_form;
mod content_type_view;
mod details;
mod details_view;
mod entries_table;
mod error_details;
mod file_ops;
mod i18n;
mod listing;
mod local_copy;
mod location;
mod moves;
mod name_dialog;
mod object_actions;
mod object_view;
mod pane;
mod places;
mod search_view;
mod settings;
mod share_view;
mod sync_view;
mod task_center;
mod tasks;
mod transfer_plan;
mod transfers_view;
mod versions_view;
mod workspace;

use app_state::AppBackend;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use settings::Settings;
use workspace::Workspace;

rust_i18n::i18n!("locales", fallback = "en");

actions!(
    supas3,
    [
        About,
        Quit,
        CloseWindow,
        Minimize,
        ToggleSidebar,
        ToggleDetails,
        NewFolder,
        RenameSelection,
        FilterRows
    ]
);

/// The main window's workspace, for application actions.
struct MainWorkspace(WeakEntity<Workspace>);

impl Global for MainWorkspace {}

/// Runs `action` on the main window's workspace, if it is open.
fn with_workspace(
    cx: &mut App,
    action: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    let Some(workspace) = cx
        .try_global::<MainWorkspace>()
        .and_then(|main| main.0.upgrade())
    else {
        return;
    };
    with_front_window(cx, move |window, cx| {
        workspace.update(cx, |workspace, cx| action(workspace, window, cx));
    });
}

/// Leaves through the workspace, which asks about unsaved edits first.
/// Returns false when there is no workspace to ask.
fn leave_workspace(cx: &mut App, how: workspace::Leave) -> bool {
    let open = cx
        .try_global::<MainWorkspace>()
        .is_some_and(|main| main.0.upgrade().is_some());
    if open {
        with_workspace(cx, move |workspace, window, cx| {
            workspace.leave(how, window, cx)
        });
    }
    open
}

/// Stops the backend, then ends the program. Every way of quitting ends here.
pub(crate) fn quit(cx: &mut App) {
    AppBackend::shutdown(cx);
    cx.quit();
}

/// Whether a dialog is open in any window. A dialog may hold unsaved edits
/// and guards its own closing, so the app is not closed behind it.
fn dialog_open(cx: &mut App) -> bool {
    cx.windows().into_iter().any(|window| {
        window
            .update(cx, |_, window, cx| window.has_active_dialog(cx))
            .unwrap_or(false)
    })
}

/// Says why quitting or closing did nothing.
fn explain_dialog_open(cx: &mut App) {
    with_front_window(cx, |window, cx| {
        window.push_notification(
            (
                gpui_kit::component::notification::NotificationType::Info,
                SharedString::from(rust_i18n::t!("menu.close_dialog_first").into_owned()),
            ),
            cx,
        );
    });
}

/// The application menu. macOS shows it in the menu bar, and it is what gives
/// the program its Quit command there; the labels follow the chosen language.
pub fn set_app_menus(cx: &mut App) {
    use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    use rust_i18n::t;
    cx.set_menus([
        Menu::new("supaS3").items([
            MenuItem::action(t!("about.menu").into_owned(), About),
            MenuItem::separator(),
            MenuItem::action(t!("menu.quit").into_owned(), Quit),
        ]),
        Menu::new(t!("menu.file").into_owned()).items([
            MenuItem::action(t!("menu.new_folder").into_owned(), NewFolder),
            MenuItem::action(t!("menu.rename").into_owned(), RenameSelection),
        ]),
        Menu::new(t!("menu.edit").into_owned()).items([
            MenuItem::os_action(t!("menu.undo").into_owned(), Undo, OsAction::Undo),
            MenuItem::os_action(t!("menu.redo").into_owned(), Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(t!("menu.cut").into_owned(), Cut, OsAction::Cut),
            MenuItem::os_action(t!("menu.copy").into_owned(), Copy, OsAction::Copy),
            MenuItem::os_action(t!("menu.paste").into_owned(), Paste, OsAction::Paste),
            MenuItem::os_action(
                t!("menu.select_all").into_owned(),
                SelectAll,
                OsAction::SelectAll,
            ),
        ]),
        Menu::new(t!("menu.view").into_owned()).items([
            MenuItem::action(t!("menu.toggle_sidebar").into_owned(), ToggleSidebar),
            MenuItem::action(t!("menu.toggle_details").into_owned(), ToggleDetails),
            MenuItem::separator(),
            MenuItem::action(t!("menu.filter").into_owned(), FilterRows),
        ]),
        Menu::new(t!("menu.window").into_owned()).items([
            MenuItem::action(t!("menu.minimize").into_owned(), Minimize),
            MenuItem::action(t!("menu.close_window").into_owned(), CloseWindow),
        ]),
    ]);
}

/// Runs `action` on the window in front, if there is one. It runs once the
/// current event is handled: an action from a key press arrives while the
/// window is being updated, when it cannot be updated again.
fn with_front_window(cx: &mut App, action: impl FnOnce(&mut Window, &mut App) + 'static) {
    cx.defer(move |cx| {
        if let Some(window) = cx.active_window().or_else(|| cx.windows().first().copied()) {
            let _ = window.update(cx, |_, window, cx| action(window, cx));
        }
    });
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,supas3=info,supas3_core=info"),
    )
    .init();

    install_panic_log();

    let config_dir = backend::default_config_dir();
    let settings = config_dir
        .as_ref()
        .map(|dir| Settings::load(dir))
        .unwrap_or_default();
    // Transfer events wait here until the task center collects them.
    let events = std::sync::Arc::new(tasks::EventQueue::default());
    let sink = events.sink();
    let started = config_dir.and_then(|dir| backend::Backend::start(dir, sink));
    if let Ok(backend) = &started {
        backend.apply_transfer_limits(settings.transfers);
    }

    let application = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    // macOS keeps a program in the Dock while it is ending. If its icon is
    // clicked while no window is left, show the window again when the backend
    // still runs, and otherwise finish quitting so the next click starts anew.
    let reopen_events = events.clone();
    application.on_reopen(move |cx| {
        if !cx.windows().is_empty() {
            return;
        }
        if AppBackend::requests(cx).is_some() {
            // Preferences changed since launch are on disk.
            let settings = backend::default_config_dir()
                .map(|dir| Settings::load(&dir))
                .unwrap_or(settings);
            open_main_window(settings, reopen_events.clone(), cx);
        } else {
            cx.quit();
        }
    });
    application.run(move |cx| {
        gpui_kit::init(cx);
        i18n::apply(settings.language);
        cx.set_global(AppBackend::new(started));
        cx.set_global(app_jobs::AppJobs::default());

        // Keys are bound before the menus are built, so the menus show them.
        bind_keys(cx);
        set_app_menus(cx);
        // Both checks read the windows, which a key press is still updating;
        // they run once it has been handled.
        cx.on_action(|_: &Quit, cx| {
            cx.defer(|cx| {
                if dialog_open(cx) {
                    explain_dialog_open(cx);
                } else if !leave_workspace(cx, workspace::Leave::Quit) {
                    quit(cx);
                }
            });
        });
        // Quitting from the Dock or when the session ends does not go
        // through the Quit action; stop the backend there too. Stopping it
        // twice does nothing.
        cx.on_app_quit(|cx| {
            AppBackend::shutdown(cx);
            async {}
        })
        .detach();
        cx.on_action(|_: &About, cx| with_front_window(cx, Workspace::show_about));
        cx.on_action(|_: &CloseWindow, cx| {
            cx.defer(|cx| {
                if dialog_open(cx) {
                    explain_dialog_open(cx);
                } else if !leave_workspace(cx, workspace::Leave::CloseWindow) {
                    with_front_window(cx, |window, _| window.remove_window());
                }
            });
        });
        cx.on_action(|_: &ToggleSidebar, cx| {
            with_workspace(cx, |workspace, window, cx| {
                workspace.toggle_sidebar(window, cx)
            });
        });
        cx.on_action(|_: &ToggleDetails, cx| {
            with_workspace(cx, |workspace, window, cx| {
                workspace.toggle_details(window, cx)
            });
        });
        fn on_pane(cx: &mut App, command: workspace::PaneCommand) {
            with_workspace(cx, move |workspace, window, cx| {
                workspace.pane_command(command, window, cx)
            });
        }
        cx.on_action(|_: &NewFolder, cx| on_pane(cx, workspace::PaneCommand::NewFolder));
        cx.on_action(|_: &RenameSelection, cx| on_pane(cx, workspace::PaneCommand::Rename));
        cx.on_action(|_: &FilterRows, cx| on_pane(cx, workspace::PaneCommand::Filter));
        cx.on_action(|_: &Minimize, cx| {
            with_front_window(cx, |window, _| window.minimize_window());
        });
        // Come to the front when started from the Dock or a launcher.
        cx.activate(true);

        // The app has one window; closing it ends the program on every
        // platform. The backend is stopped here rather than left to a
        // destructor, because not every platform returns from `run`.
        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                quit(cx);
            }
        })
        .detach();

        open_main_window(settings, events.clone(), cx);

        // For launch checks: with SUPAS3_EXIT_AFTER_LAUNCH set to "close" or
        // "quit", the program ends itself that way a few seconds after its
        // window opened, so a check can tell a clean exit from a hang.
        let how = std::env::var("SUPAS3_EXIT_AFTER_LAUNCH").ok();
        if let Some(how) = how.filter(|how| how == "close" || how == "quit") {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
                cx.update(|cx| {
                    if how == "close" {
                        with_front_window(cx, |window, _| window.remove_window());
                    } else {
                        quit(cx);
                    }
                });
            })
            .detach();
        }
    });
}

/// The app's shortcuts. "secondary" is Command on macOS and Control
/// elsewhere.
fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-i", ToggleDetails, None),
        KeyBinding::new("secondary-shift-n", NewFolder, None),
        KeyBinding::new("f2", RenameSelection, None),
        KeyBinding::new("secondary-f", FilterRows, None),
        KeyBinding::new("tab", workspace::FocusNextRegion, Some("Workspace")),
        KeyBinding::new(
            "shift-tab",
            workspace::FocusPreviousRegion,
            Some("Workspace"),
        ),
        // Tables would take Tab to move between columns; in a pane it
        // leaves the pane instead.
        KeyBinding::new(
            "tab",
            workspace::FocusNextRegion,
            Some("Workspace > DataTable"),
        ),
        KeyBinding::new(
            "shift-tab",
            workspace::FocusPreviousRegion,
            Some("Workspace > DataTable"),
        ),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([KeyBinding::new("cmd-m", Minimize, None)]);
}

fn open_main_window(settings: Settings, events: std::sync::Arc<tasks::EventQueue>, cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1200.), px(760.)), cx)),
        window_min_size: Some(size(px(860.), px(520.))),
        // Matches the installed desktop entry, so the running window is
        // shown under the application's own icon and launcher.
        app_id: Some("supas3".into()),
        titlebar: Some(TitlebarOptions {
            title: Some("supaS3".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let (_, workspace) = gpui_kit::open_window(options, cx, move |window, cx| {
        window.activate_window();
        cx.new(|cx| Workspace::new(settings, events.clone(), window, cx))
    })
    .expect("failed to open the main window");
    cx.set_global(MainWorkspace(workspace.downgrade()));
}

/// Records panics in `panic.log` in the configuration directory as well as the
/// log, because a release build on Windows has no console to print to.
fn install_panic_log() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = format!(
            "{info}\nBacktrace:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        log::error!("{message}");
        if let Ok(dir) = backend::default_config_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("panic.log"), &message);
        }
        previous(info);
    }));
}
