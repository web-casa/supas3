// SPDX-License-Identifier: AGPL-3.0-or-later
//! supaS3 desktop application.

// Release builds on Windows are GUI programs and must not open a console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_state;
mod backend;
mod connection;
mod connection_form;
mod entries_table;
mod i18n;
mod listing;
mod location;
mod object_actions;
mod object_view;
mod pane;
mod settings;
mod share_view;
mod sync_view;
mod task_center;
mod tasks;
mod transfer_plan;
mod versions_view;
mod workspace;

use app_state::AppBackend;
use gpui_kit::*;
use settings::Settings;
use workspace::Workspace;

rust_i18n::i18n!("locales", fallback = "en");

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

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            i18n::apply(settings.language);
            cx.set_global(AppBackend::new(started));

            // The app has one window; closing it ends the program on every
            // platform. The backend is stopped here rather than left to a
            // destructor, because not every platform returns from `run`.
            cx.on_window_closed(|cx, _window_id| {
                if cx.windows().is_empty() {
                    AppBackend::shutdown(cx);
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1200.), px(760.)), cx)),
                window_min_size: Some(size(px(860.), px(520.))),
                // Matches the installed desktop entry, so the running window
                // is shown under the application's own icon and launcher.
                app_id: Some("supas3".into()),
                titlebar: Some(TitlebarOptions {
                    title: Some("supaS3".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            gpui_kit::open_window(options, cx, move |window, cx| {
                window.activate_window();
                cx.new(|cx| Workspace::new(settings, events.clone(), window, cx))
            })
            .expect("failed to open the main window");
        });
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
