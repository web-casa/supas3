// SPDX-License-Identifier: AGPL-3.0-or-later
//! The main window: connections on the side, two panes of tabs beside them.

use crate::app_state::{AppBackend, Connections};
use crate::backend::default_config_dir;
use crate::connection_form::{ConnectionForm, ConnectionFormEvent};
use crate::i18n;
use crate::location::Location;
use crate::pane::{Pane, PaneEvent};
use crate::settings::{Appearance, Language, Settings};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::NotificationType;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::sidebar::{Sidebar, SidebarGroup, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme as _, IconName, Sizable as _, StyledExt as _, Theme, ThemeMode,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use supas3_core::credentials::Profile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

pub struct Workspace {
    connections: Entity<Connections>,
    left: Entity<Pane>,
    right: Entity<Pane>,
    /// The pane that sidebar choices open in.
    active: Side,
    settings: Settings,
    /// Kept alive while its dialog is open.
    form: Option<Entity<ConnectionForm>>,
    /// A problem that is not tied to one pane, shown above the panes.
    notice: Option<String>,
    /// Counts connection reloads, so only the latest answer is applied.
    reload_generation: u64,
    _subscriptions: Vec<Subscription>,
}

/// The folder a local pane starts in.
fn home() -> Location {
    Location::Local(dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/")))
}

/// Where a connection opens: its default bucket when it has one, otherwise
/// its list of buckets.
fn start_of(profile: &Profile) -> Location {
    match &profile.compat.default_bucket {
        Some(bucket) => Location::objects(
            &profile.id,
            bucket,
            profile.compat.default_prefix.as_deref().unwrap_or(""),
        ),
        None => Location::Buckets {
            profile_id: profile.id.clone(),
        },
    }
}

impl Workspace {
    pub fn new(settings: Settings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connections = cx.new(|_| Connections::default());
        let left = cx.new(|cx| Pane::new("left", home(), connections.clone(), window, cx));
        let right = cx.new(|cx| Pane::new("right", home(), connections.clone(), window, cx));
        let subscriptions = vec![
            cx.subscribe(&left, |workspace, _, _: &PaneEvent, cx| {
                workspace.activate(Side::Left, cx);
            }),
            cx.subscribe(&right, |workspace, _, _: &PaneEvent, cx| {
                workspace.activate(Side::Right, cx);
            }),
            cx.observe(&connections, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |workspace, window, cx| {
                if workspace.settings.appearance == Appearance::System {
                    Theme::sync_system_appearance(Some(window), cx);
                }
            }),
        ];
        let mut workspace = Self {
            connections,
            left,
            right,
            active: Side::Right,
            settings,
            form: None,
            notice: AppBackend::start_error(cx)
                .map(|error| t!("error.backend_failed", error = error).into_owned()),
            reload_generation: 0,
            _subscriptions: subscriptions,
        };
        workspace.apply_appearance(window, cx);
        workspace.reload_connections(true, cx);
        workspace
    }

    fn pane(&self, side: Side) -> &Entity<Pane> {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    fn activate(&mut self, side: Side, cx: &mut Context<Self>) {
        if self.active != side {
            self.active = side;
            cx.notify();
        }
    }

    /// Reads the saved connections. On first load the right pane opens the
    /// first connection, so the window starts as "this computer | storage",
    /// unless the user has already gone somewhere in it.
    fn reload_connections(&mut self, first_load: bool, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        self.reload_generation += 1;
        let generation = self.reload_generation;
        cx.spawn(async move |workspace, cx| {
            let result = requests.profiles().await;
            let _ = workspace.update(cx, |workspace, cx| {
                // A slower, earlier read must not replace a newer one.
                if generation != workspace.reload_generation {
                    return;
                }
                match result {
                    Ok(profiles) => {
                        workspace.connections.update(cx, |connections, cx| {
                            connections.set(profiles);
                            cx.notify();
                        });
                        let first = workspace
                            .connections
                            .read(cx)
                            .profiles()
                            .first()
                            .map(start_of);
                        let untouched = workspace.right.read(cx).is_untouched();
                        if let (true, true, Some(location)) = (first_load, untouched, first) {
                            workspace
                                .right
                                .update(cx, |pane, cx| pane.open(location, cx));
                        }
                    }
                    Err(error) => {
                        workspace.notice =
                            Some(t!("connection.load_failed", detail = error).into_owned());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn open_in_active(&mut self, location: Location, cx: &mut Context<Self>) {
        self.pane(self.active)
            .clone()
            .update(cx, |pane, cx| pane.open(location, cx));
    }

    fn show_form(
        &mut self,
        existing: Option<Profile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title: SharedString = match &existing {
            Some(profile) => t!("connection.edit_title", name = profile.name)
                .into_owned()
                .into(),
            None => t!("connection.add_title").into_owned().into(),
        };
        let editing = existing.is_some();
        let form = cx.new(|cx| ConnectionForm::new(existing, window, cx));
        cx.subscribe_in(&form, window, move |workspace, form, event, window, cx| {
            // The dialog on top belongs to this form only while the workspace
            // still holds it; an answer that arrives after the form was
            // closed must not close whatever is open now.
            let is_open = workspace
                .form
                .as_ref()
                .is_some_and(|open| open.entity_id() == form.entity_id());
            if is_open {
                window.close_dialog(cx);
                workspace.form = None;
            }
            if let ConnectionFormEvent::Saved(profile) = event {
                workspace.reload_connections(false, cx);
                if editing {
                    // Tabs already showing this connection hold rows and
                    // paging tokens fetched with the old settings.
                    for pane in [workspace.left.clone(), workspace.right.clone()] {
                        pane.update(cx, |pane, cx| pane.reload_profile(&profile.id, cx));
                    }
                } else if is_open {
                    workspace.open_in_active(start_of(profile), cx);
                }
            }
        })
        .detach();
        self.form = Some(form.clone());
        let workspace = cx.weak_entity();
        let width = rems(34.).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, _| {
            let submit = form.clone();
            let (closed_form, closed_workspace) = (form.clone(), workspace.clone());
            dialog
                .title(title.clone())
                .width(width)
                .child(form.clone())
                // Enter saves through the form, which closes the dialog only
                // after the connection has been stored.
                .on_ok(move |_, _, cx| {
                    submit.update(cx, |form, cx| form.save(cx));
                    false
                })
                // Escape, the close button and a click outside all end here:
                // let go of the form so it is not kept alive behind the scenes.
                .on_close(move |_, _, cx| {
                    let _ = closed_workspace.update(cx, |workspace, _| {
                        let is_this = workspace
                            .form
                            .as_ref()
                            .is_some_and(|open| open.entity_id() == closed_form.entity_id());
                        if is_this {
                            workspace.form = None;
                        }
                    });
                })
        });
    }

    fn confirm_delete(&mut self, profile: Profile, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.weak_entity();
        let title: SharedString = t!("connection.delete_title", name = profile.name)
            .into_owned()
            .into();
        window.open_dialog(cx, move |dialog, _, _| {
            let workspace = workspace.clone();
            let profile_id = profile.id.clone();
            dialog
                .title(title.clone())
                .child(t!("connection.delete_body").into_owned())
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-delete")
                                .outline()
                                .label(t!("action.cancel").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-delete")
                                .danger()
                                .label(t!("action.delete").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let profile_id = profile_id.clone();
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        workspace.delete_connection(profile_id, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn delete_connection(&mut self, profile_id: String, cx: &mut Context<Self>) {
        let Some(requests) = AppBackend::requests(cx) else {
            return;
        };
        cx.spawn(async move |workspace, cx| {
            let result = requests.delete_profile(profile_id.clone()).await;
            let _ = workspace.update(cx, |workspace, cx| match result {
                Ok(()) => {
                    // Tabs showing the removed connection have nothing to show.
                    for pane in [workspace.left.clone(), workspace.right.clone()] {
                        pane.update(cx, |pane, cx| pane.leave_profile(&profile_id, &home(), cx));
                    }
                    workspace.reload_connections(false, cx);
                }
                Err(error) => {
                    workspace.notice =
                        Some(t!("connection.delete_failed", detail = error).into_owned());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_appearance(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.settings.appearance {
            Appearance::System => Theme::sync_system_appearance(Some(window), cx),
            Appearance::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            Appearance::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
    }

    fn change_settings(
        &mut self,
        change: impl FnOnce(&mut Settings),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        change(&mut self.settings);
        i18n::apply(self.settings.language);
        self.apply_appearance(window, cx);
        // Column titles are fetched once per table; fetch them again.
        for pane in [self.left.clone(), self.right.clone()] {
            pane.update(cx, |pane, cx| pane.refresh_labels(cx));
        }
        let saved = default_config_dir()
            .map_err(|error| error.to_string())
            .and_then(|dir| self.settings.save(&dir).map_err(|error| error.to_string()));
        if let Err(error) = saved {
            log::warn!("could not save interface preferences: {error}");
            // The choice is applied; say that it will not survive a restart.
            window.push_notification(
                (
                    NotificationType::Warning,
                    SharedString::from(t!("preferences.not_saved").into_owned()),
                ),
                cx,
            );
        }
        cx.refresh_windows();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_location = self.pane(self.active).read(cx).location().clone();
        let connections = self.connections.read(cx).profiles().to_vec();
        let workspace = cx.weak_entity();
        let settings = self.settings;

        let this_computer = SidebarMenuItem::new(t!("location.home").into_owned())
            .icon(IconName::HardDrive)
            .active(matches!(active_location, Location::Local(_)))
            .on_click(cx.listener(|workspace, _, _, cx| workspace.open_in_active(home(), cx)));

        let connection_items = connections.into_iter().map(|profile| {
            let is_active = active_location.profile_id() == Some(profile.id.as_str());
            let open = profile.clone();
            let menu_profile = profile.clone();
            let menu_workspace = workspace.clone();
            SidebarMenuItem::new(profile.name.clone())
                .icon(IconName::Globe)
                .active(is_active)
                .on_click(cx.listener(move |workspace, _, _, cx| {
                    workspace.open_in_active(start_of(&open), cx);
                }))
                .suffix(move |_, _| {
                    let profile = menu_profile.clone();
                    let workspace = menu_workspace.clone();
                    Button::new(SharedString::from(format!("actions-{}", profile.id)))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Ellipsis)
                        .tooltip(t!("connection.actions", name = profile.name).into_owned())
                        .dropdown_menu(move |menu, _, _| {
                            let (edit_profile, delete_profile) = (profile.clone(), profile.clone());
                            let (edit_workspace, delete_workspace) =
                                (workspace.clone(), workspace.clone());
                            menu.item(
                                PopupMenuItem::new(t!("connection.edit").into_owned()).on_click(
                                    move |_, window, cx| {
                                        let profile = edit_profile.clone();
                                        let _ = edit_workspace.update(cx, |workspace, cx| {
                                            workspace.show_form(Some(profile), window, cx);
                                        });
                                    },
                                ),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(t!("connection.delete").into_owned()).on_click(
                                    move |_, window, cx| {
                                        let profile = delete_profile.clone();
                                        let _ = delete_workspace.update(cx, |workspace, cx| {
                                            workspace.confirm_delete(profile, window, cx);
                                        });
                                    },
                                ),
                            )
                        })
                })
        });
        let has_connections = !self.connections.read(cx).profiles().is_empty();

        Sidebar::new("sidebar")
            .header(div().px_2().py_1().font_semibold().child("supaS3"))
            .child(
                SidebarGroup::new(t!("sidebar.places").into_owned())
                    .child(SidebarMenu::new().child(this_computer)),
            )
            .child(
                SidebarGroup::new(t!("sidebar.connections").into_owned())
                    .child(SidebarMenu::new().children(connection_items)),
            )
            .footer(
                v_flex()
                    .w_full()
                    .gap_2()
                    .when(!has_connections, |this| {
                        this.child(
                            div()
                                .px_2()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("sidebar.no_connections").into_owned()),
                        )
                    })
                    .child(
                        Button::new("add-connection")
                            .w_full()
                            .icon(IconName::Plus)
                            .label(t!("sidebar.add_connection").into_owned())
                            .on_click(cx.listener(|workspace, _, window, cx| {
                                workspace.show_form(None, window, cx);
                            })),
                    )
                    .child(
                        Button::new("preferences")
                            .w_full()
                            .ghost()
                            .icon(IconName::Settings)
                            .label(t!("sidebar.preferences").into_owned())
                            .dropdown_menu(move |menu, _, _| {
                                let pick = |workspace: &WeakEntity<Workspace>,
                                            change: fn(&mut Settings)| {
                                    let workspace = workspace.clone();
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = workspace.update(cx, |workspace, cx| {
                                            workspace.change_settings(change, window, cx);
                                        });
                                    }
                                };
                                menu.label(t!("preferences.language").into_owned())
                                    .item(
                                        PopupMenuItem::new(
                                            t!("preferences.follow_system").into_owned(),
                                        )
                                        .checked(settings.language == Language::System)
                                        .on_click(pick(&workspace, |s| {
                                            s.language = Language::System
                                        })),
                                    )
                                    .item(
                                        PopupMenuItem::new("English")
                                            .checked(settings.language == Language::English)
                                            .on_click(pick(&workspace, |s| {
                                                s.language = Language::English
                                            })),
                                    )
                                    .item(
                                        PopupMenuItem::new("简体中文")
                                            .checked(
                                                settings.language == Language::SimplifiedChinese,
                                            )
                                            .on_click(pick(&workspace, |s| {
                                                s.language = Language::SimplifiedChinese
                                            })),
                                    )
                                    .separator()
                                    .label(t!("preferences.appearance").into_owned())
                                    .item(
                                        PopupMenuItem::new(
                                            t!("preferences.follow_system").into_owned(),
                                        )
                                        .checked(settings.appearance == Appearance::System)
                                        .on_click(pick(&workspace, |s| {
                                            s.appearance = Appearance::System
                                        })),
                                    )
                                    .item(
                                        PopupMenuItem::new(t!("preferences.light").into_owned())
                                            .checked(settings.appearance == Appearance::Light)
                                            .on_click(pick(&workspace, |s| {
                                                s.appearance = Appearance::Light
                                            })),
                                    )
                                    .item(
                                        PopupMenuItem::new(t!("preferences.dark").into_owned())
                                            .checked(settings.appearance == Appearance::Dark)
                                            .on_click(pick(&workspace, |s| {
                                                s.appearance = Appearance::Dark
                                            })),
                                    )
                            }),
                    ),
            )
    }

    fn render_pane(&self, side: Side, cx: &mut Context<Self>) -> impl IntoElement {
        // The active pane is outlined; sidebar choices open there.
        let outline = if self.active == side {
            cx.theme().ring
        } else {
            cx.theme().transparent
        };
        div()
            .size_full()
            .border_1()
            .border_color(outline)
            .child(self.pane(side).clone())
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Narrower than this, a pane's navigation bar no longer fits.
        let min_pane = rems(18.).to_pixels(window.rem_size());
        h_flex()
            .items_stretch()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .when_some(self.notice.clone(), |this, notice| {
                        this.child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(notice),
                        )
                    })
                    .child(
                        div().flex_1().min_h_0().child(
                            h_resizable("panes")
                                .child(
                                    resizable_panel()
                                        .size_range(min_pane..Pixels::MAX)
                                        .child(self.render_pane(Side::Left, cx)),
                                )
                                .child(
                                    resizable_panel()
                                        .size_range(min_pane..Pixels::MAX)
                                        .child(self.render_pane(Side::Right, cx)),
                                ),
                        ),
                    ),
            )
    }
}
