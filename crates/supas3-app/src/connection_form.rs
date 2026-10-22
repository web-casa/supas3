// SPDX-License-Identifier: AGPL-3.0-or-later
//! The form for adding or editing a connection.

use crate::app_state::AppBackend;
use crate::aws_profiles::{self, LocalProfile};
use crate::connection::{Draft, Problem, Service};
use crate::error_details::ErrorDetails;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, IndexPath};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use supas3_core::credentials::Profile;

/// What the form asks its owner to do.
pub enum ConnectionFormEvent {
    /// A connection was stored.
    Saved(Box<Profile>),
    /// The user backed out without saving.
    Cancelled,
}

/// The outcome of a connection test, together with the values it tested. A
/// result is only shown while the form still holds those values, so an old
/// "connected" never vouches for settings that were edited afterwards.
enum Check {
    NotRun,
    Running,
    Passed(Draft),
    Failed(Draft, String),
}

pub struct ConnectionForm {
    /// The connection being edited; `None` when adding one.
    existing: Option<Profile>,
    service: Service,
    service_select: Entity<SelectState<Vec<SharedString>>>,
    name: Entity<InputState>,
    region: Entity<InputState>,
    endpoint: Entity<InputState>,
    bucket: Entity<InputState>,
    access_key: Entity<InputState>,
    secret_key: Entity<InputState>,
    session_token: Entity<InputState>,
    /// For AWS: typed keys, or one of the profiles below.
    credential_select: Entity<SelectState<Vec<SharedString>>>,
    /// The profiles of this computer's AWS configuration, `None` when it has
    /// none; the usable ones follow "typed keys" in the select.
    local_profiles: Option<Vec<LocalProfile>>,
    /// The profile chosen in the select, if any.
    aws_profile: Option<String>,
    /// Shown once the user has tried to save or test.
    problems: Vec<Problem>,
    check: Check,
    saving: bool,
    save_error: Option<String>,
}

impl EventEmitter<ConnectionFormEvent> for ConnectionForm {}

impl ConnectionForm {
    pub fn new(existing: Option<Profile>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let draft = existing
            .as_ref()
            .map_or_else(|| Draft::new(Service::Aws), Draft::from_profile);
        let mut input = |value: &str, masked: bool| {
            let value = value.to_string();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value)
                    .masked(masked)
            })
        };
        let name = input(&draft.name, false);
        let region = input(&draft.region, false);
        let endpoint = input(&draft.endpoint, false);
        let bucket = input(&draft.bucket, false);
        let access_key = input(&draft.access_key, false);
        let secret_key = input("", true);
        let session_token = input("", true);

        let services: Vec<SharedString> = Service::ALL
            .iter()
            .map(|service| t!(service.label_key()).into_owned().into())
            .collect();
        let selected = Service::ALL
            .iter()
            .position(|service| *service == draft.service)
            .map(|row| IndexPath::default().row(row));
        let service_select = cx.new(|cx| SelectState::new(services, selected, window, cx));

        let local_profiles = aws_profiles::discover();
        // The usable profiles, and the one being edited even if it is gone
        // from the files, so the form shows what the connection uses.
        let mut usable: Vec<String> = local_profiles
            .iter()
            .flatten()
            .filter(|profile| profile.unusable.is_none())
            .map(|profile| profile.name.clone())
            .collect();
        if let Some(name) = &draft.aws_profile {
            if !usable.contains(name) {
                usable.push(name.clone());
            }
        }
        let mut sources: Vec<SharedString> = vec![t!("form.credentials_typed").into_owned().into()];
        sources.extend(usable.iter().map(|name| {
            SharedString::from(t!("form.credentials_profile", name = name).into_owned())
        }));
        let selected_source = draft
            .aws_profile
            .as_ref()
            .and_then(|name| usable.iter().position(|usable| usable == name))
            .map_or(0, |index| index + 1);
        let credential_select = cx.new(|cx| {
            SelectState::new(
                sources,
                Some(IndexPath::default().row(selected_source)),
                window,
                cx,
            )
        });
        cx.subscribe_in(
            &credential_select,
            window,
            move |form, select, _: &SelectEvent<Vec<SharedString>>, window, cx| {
                let row = select.read(cx).selected_index(cx).map(|index| index.row);
                form.aws_profile = row
                    .and_then(|row| row.checked_sub(1))
                    .and_then(|index| usable.get(index).cloned());
                form.problems.clear();
                // Fill what the profile tells, without replacing anything typed.
                if let Some(name) = form.aws_profile.clone() {
                    let region = form
                        .local_profiles
                        .iter()
                        .flatten()
                        .find(|profile| profile.name == name)
                        .and_then(|profile| profile.region.clone());
                    for (input, value) in [(&form.name, Some(name)), (&form.region, region)] {
                        if let Some(value) = value {
                            input.update(cx, |input, cx| {
                                if input.value().trim().is_empty() {
                                    input.set_value(value, window, cx);
                                }
                            });
                        }
                    }
                }
                cx.notify();
            },
        )
        .detach();
        cx.subscribe(
            &service_select,
            |form, select, _: &SelectEvent<Vec<SharedString>>, cx| {
                let row = select.read(cx).selected_index(cx).map(|index| index.row);
                if let Some(service) = row.and_then(|row| Service::ALL.get(row)) {
                    form.service = *service;
                    form.problems.clear();
                    cx.notify();
                }
            },
        )
        .detach();

        Self {
            service: draft.service,
            service_select,
            name,
            region,
            endpoint,
            bucket,
            access_key,
            secret_key,
            session_token,
            credential_select,
            local_profiles,
            aws_profile: draft.aws_profile.clone(),
            existing,
            problems: Vec::new(),
            check: Check::NotRun,
            saving: false,
            save_error: None,
        }
    }

    fn draft(&self, cx: &App) -> Draft {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        Draft {
            service: self.service,
            name: text(&self.name),
            region: text(&self.region),
            endpoint: text(&self.endpoint),
            access_key: text(&self.access_key),
            secret_key: text(&self.secret_key),
            session_token: text(&self.session_token),
            bucket: text(&self.bucket),
            aws_profile: self.aws_profile.clone(),
        }
    }

    /// What the credential select's help says: where profiles come from,
    /// and which were left out and why.
    fn credentials_help(&self) -> String {
        // The profile this connection uses, if it is no longer usable.
        if let Some(name) = &self.aws_profile {
            let found = self
                .local_profiles
                .iter()
                .flatten()
                .find(|profile| &profile.name == name);
            match found {
                None => return t!("form.aws_profile_gone", name = name).into_owned(),
                Some(profile) => {
                    if let Some(why) = profile.unusable {
                        return t!(
                            "form.aws_profile_unusable",
                            name = name,
                            why = t!(why.message_key())
                        )
                        .into_owned();
                    }
                }
            }
        }
        let Some(profiles) = &self.local_profiles else {
            return t!("form.aws_config_missing").into_owned();
        };
        let left_out: Vec<String> = profiles
            .iter()
            .filter_map(|profile| {
                let why = profile.unusable?;
                Some(
                    t!(
                        "form.aws_profile_left_out",
                        name = profile.name,
                        why = t!(why.message_key())
                    )
                    .into_owned(),
                )
            })
            .collect();
        if left_out.is_empty() {
            t!("form.aws_config_help").into_owned()
        } else {
            let separator = t!("form.list_separator");
            t!(
                "form.aws_config_left_out",
                profiles = left_out.join(&separator)
            )
            .into_owned()
        }
    }

    /// The profile to test or save, or `None` after recording what is missing.
    fn validated(&mut self, cx: &mut Context<Self>) -> Option<Profile> {
        let draft = self.draft(cx);
        self.problems = draft.problems(self.existing.is_some());
        cx.notify();
        self.problems
            .is_empty()
            .then(|| draft.to_profile(self.existing.as_ref()))
    }

    /// Whether a test or a save is in flight; both are refused meanwhile.
    fn is_busy(&self) -> bool {
        self.saving || matches!(self.check, Check::Running)
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(profile) = self.validated(cx) else {
            return;
        };
        let tested = self.draft(cx);
        let Some(requests) = AppBackend::requests(cx) else {
            self.check = Check::Failed(tested, t!("error.backend_stopped").into_owned());
            return;
        };
        self.check = Check::Running;
        cx.spawn(async move |form, cx| {
            let result = requests.check_profile(profile).await;
            let _ = form.update(cx, |form, cx| {
                form.check = match result {
                    Ok(()) => Check::Passed(tested),
                    Err(message) => Check::Failed(tested, message),
                };
                cx.notify();
            });
        })
        .detach();
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(profile) = self.validated(cx) else {
            return;
        };
        let Some(requests) = AppBackend::requests(cx) else {
            self.save_error = Some(t!("error.backend_stopped").into_owned());
            return;
        };
        self.saving = true;
        self.save_error = None;
        let editing = self.existing.is_some();
        cx.spawn(async move |form, cx| {
            let result = requests.save_profile(profile, editing).await;
            let _ = form.update(cx, |form, cx| {
                form.saving = false;
                match result {
                    Ok(saved) => cx.emit(ConnectionFormEvent::Saved(Box::new(saved))),
                    Err(message) => form.save_error = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The message for a field's problem, if it has one.
    fn problem(&self, among: &[Problem]) -> Option<String> {
        self.problems
            .iter()
            .find(|problem| among.contains(problem))
            .map(|problem| t!(problem.message_key()).into_owned())
    }

    fn field(
        &self,
        label: &str,
        input: &Entity<InputState>,
        help: Option<String>,
        problems: &[Problem],
        cx: &App,
    ) -> Field {
        let problem = self.problem(problems);
        let danger = cx.theme().danger;
        let field = Field::new()
            .label(t!(label).into_owned())
            .child(Input::new(input));
        match (problem, help) {
            (Some(problem), _) => {
                field.description_fn(move |_, _| div().text_color(danger).child(problem.clone()))
            }
            (None, Some(help)) => field.description(help),
            (None, None) => field,
        }
    }
}

impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let draft = self.draft(cx);
        let is_aws = self.service == Service::Aws;
        let typed_keys = !draft.uses_aws_profile();
        let endpoint_help = match self.service.endpoint_for(&draft.region) {
            Some(endpoint) => Some(t!("form.endpoint_default", endpoint = endpoint).into_owned()),
            None if self.service == Service::Generic => None,
            None => Some(t!("form.endpoint_from_region").into_owned()),
        };
        let secret_help = self
            .existing
            .is_some()
            .then(|| t!("form.secret_keep").into_owned());
        let busy = self.is_busy();

        // A finished result is shown only for the values it was run against.
        let (check_text, check_error) = match &self.check {
            Check::Running => (Some(t!("form.testing").into_owned()), None),
            Check::Passed(tested) if *tested == draft => {
                (Some(t!("form.test_passed").into_owned()), None)
            }
            Check::Failed(tested, message) if *tested == draft => (
                Some(t!("form.test_failed_title").into_owned()),
                Some(message.clone()),
            ),
            _ => (None, None),
        };
        let check_failed = check_error.is_some();

        v_flex()
            .gap_4()
            .child(
                // The fields scroll inside the dialog so the actions below
                // stay reachable in a short window.
                div()
                    .id("connection-fields")
                    .max_h(rems(26.))
                    .overflow_y_scroll()
                    .pr_2()
                    .child(
                        Form::new()
                            .child(
                                Field::new().label(t!("form.service").into_owned()).child(
                                    v_flex()
                                        .gap_2()
                                        .child(Select::new(&self.service_select))
                                        .when(!self.service.is_verified(), |this| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(t!("form.unverified").into_owned()),
                                            )
                                        }),
                                ),
                            )
                            .child(self.field(
                                "form.name",
                                &self.name,
                                None,
                                &[Problem::NameMissing],
                                cx,
                            ))
                            .child(
                                self.field(
                                    "form.region",
                                    &self.region,
                                    Some(
                                        t!(
                                            "form.region_example",
                                            example = self.service.region_example()
                                        )
                                        .into_owned(),
                                    ),
                                    &[Problem::RegionMissing],
                                    cx,
                                ),
                            )
                            .child(
                                self.field(
                                    "form.endpoint",
                                    &self.endpoint,
                                    endpoint_help,
                                    &[Problem::EndpointMissing],
                                    cx,
                                )
                                .visible(!is_aws),
                            )
                            .child(
                                Field::new()
                                    .label(t!("form.credentials").into_owned())
                                    .child(Select::new(&self.credential_select))
                                    .description(self.credentials_help())
                                    .visible(is_aws),
                            )
                            .child(
                                self.field(
                                    "form.access_key",
                                    &self.access_key,
                                    None,
                                    &[Problem::AccessKeyMissing],
                                    cx,
                                )
                                .visible(typed_keys),
                            )
                            .child(
                                self.field(
                                    "form.secret_key",
                                    &self.secret_key,
                                    secret_help,
                                    &[Problem::SecretKeyMissing],
                                    cx,
                                )
                                .visible(typed_keys),
                            )
                            .child(
                                self.field(
                                    "form.session_token",
                                    &self.session_token,
                                    Some(
                                        if self.existing.is_some() {
                                            t!("form.session_token_keep")
                                        } else {
                                            t!("form.session_token_help")
                                        }
                                        .into_owned(),
                                    ),
                                    &[],
                                    cx,
                                )
                                .visible(typed_keys),
                            )
                            .child(self.field(
                                "form.bucket",
                                &self.bucket,
                                Some(t!("form.bucket_help").into_owned()),
                                &[
                                    Problem::CosBucketNeedsAppId,
                                    Problem::OssBucketHasUnderscore,
                                    Problem::BucketHasDot,
                                ],
                                cx,
                            )),
                    ),
            )
            .when_some(check_text, |this, text| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(if check_failed {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(text),
                )
            })
            .when_some(check_error, |this, error| {
                this.child(ErrorDetails::new("check-error", error))
            })
            .when_some(self.save_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(t!("form.save_failed", detail = error).into_owned()),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("test-connection")
                            .label(t!("form.test").into_owned())
                            .disabled(busy)
                            .on_click(cx.listener(|form, _, _, cx| form.test(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-connection")
                            .outline()
                            .label(t!("action.cancel").into_owned())
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(ConnectionFormEvent::Cancelled);
                            })),
                    )
                    .child(
                        Button::new("save-connection")
                            .primary()
                            .label(t!("action.save").into_owned())
                            .loading(self.saving)
                            .disabled(busy)
                            .on_click(cx.listener(|form, _, _, cx| form.save(cx))),
                    ),
            )
    }
}
