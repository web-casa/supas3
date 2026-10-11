// SPDX-License-Identifier: AGPL-3.0-or-later
//! supaS3 desktop application.

// Release builds on Windows are GUI programs and must not open a console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod backend;
mod i18n;
mod welcome;

use gpui_kit::*;
use welcome::Welcome;

rust_i18n::i18n!("locales", fallback = "en");

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,supas3=info,supas3_core=info"),
    )
    .init();

    install_panic_log();

    let (backend, status) = backend::start();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            i18n::follow_system();
            cx.set_global(AppBackend(backend));

            // The app has one window; closing it ends the program on every
            // platform. The backend is stopped here rather than left to a
            // destructor, because not every platform returns from `run`.
            cx.on_window_closed(|cx, _window_id| {
                if cx.windows().is_empty() {
                    if let Some(mut backend) = cx.global_mut::<AppBackend>().0.take() {
                        backend.shutdown();
                    }
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(960.), px(640.)), cx)),
                window_min_size: Some(size(px(640.), px(420.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("supaS3".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                window.activate_window();
                cx.new(|_| Welcome::new(status.clone()))
            })
            .expect("failed to open the main window");
        });
}

/// The backend for the whole application; `None` if it failed to start or
/// has been shut down.
struct AppBackend(Option<backend::Backend>);

impl Global for AppBackend {}

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
