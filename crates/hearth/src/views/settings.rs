//! Settings. Seven of them, and a list of what deliberately isn't here.
//!
//! Each row is a question with two or three answers, rendered as a pill group
//! rather than a switch: the options are the documentation. A row that reads
//! "Tell me about — Everything / Mentions / Nothing" needs no help text to
//! explain what the middle setting does.

use idea_ui::{shape, size, tone, typography_kind, variant, Badge, Button, Stack, StackAxis, StackGap, ToneRef, Typography, VariantRef};
use runtime_core::{switch, component, ui, Element, StyleApplication};

use crate::model::{App, Notify};
use crate::net;
use crate::styles;
use crate::views::bits::{cb, Muted, SectionLabel, Strong};

/// Things Hearth doesn't have. Rendered as badges because a list of absences
/// reads better as a set of tags than as prose — and because the point is how
/// *many* there are.
const NOT_HERE: &[&str] = &[
    "Bots",
    "Slash commands",
    "Paid boosts",
    "A games tab",
    "Streaks",
    "Ads",
    "Discovery feed",
    "Level-up pings",
];

/// One option pill. Selected is a filled primary; the rest are ghosts.
fn option(label: &str, selected: bool, on_pick: impl Fn() + 'static) -> Element {
    ui! {
        Button(
            label = label.to_string(),
            tone = if selected { ToneRef::from(tone::Primary) } else { ToneRef::from(tone::Neutral) },
            variant = if selected {
                VariantRef::from(variant::Filled)
            } else {
                VariantRef::from(variant::Ghost)
            },
            size = size::Sm,
            shape = shape::Pill,
            on_click = cb(on_pick),
        )
    }
}

/// One settings row: name, help text, and the options on the right.
fn row(name: &str, help: &str, options: Vec<Element>) -> Element {
    ui! {
        view(style = || StyleApplication::new(styles::SettingsRow::sheet())) {
            view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
                Strong(label = name.to_string())
                Muted(label = help.to_string(), kind = typography_kind::BodySm)
            }
            Stack(axis = StackAxis::Row, gap = StackGap::Xs) {
                options
            }
        }
    }
}

/// A titled group of rows.
fn group(label: &str, rows: Vec<Element>) -> Element {
    ui! {
        view(style = || StyleApplication::new(styles::SettingsGroup::sheet())) {
            SectionLabel(label = label.to_string())
            rows
        }
    }
}

/// An on/off pair, which is most of this page.
fn on_off(current: bool, set: impl Fn(bool) + 'static) -> Vec<Element> {
    let set = std::rc::Rc::new(set);
    let on = set.clone();
    let off = set;
    vec![
        option("On", current, move || on(true)),
        option("Off", !current, move || off(false)),
    ]
}

#[component]
pub fn SettingsPage(#[prop(static)] app: App) -> Element {
    switch(move || (app.dark.get(), app.accent.get(), app.notify.get(), app.quiet_hours.get(), app.compact.get(), app.read_receipts.get(), app.mic.get(), app.cam.get()), move |_| settings_body(app))
}

fn settings_body(app: App) -> Element {
    let notify = app.notify.get();
    let compact = app.compact.get();
    let dark = app.dark.get();

    ui! {
        scroll_view(style = || StyleApplication::new(styles::SettingsPage::sheet())) {
            view(style = || StyleApplication::new(styles::SettingsColumn::sheet())) {
                view(style = || StyleApplication::new(styles::Column::sheet())) {
                    Typography(content = "Settings".to_string(), kind = typography_kind::H1)
                    Muted(label = "Seven of them. That's the whole list.".to_string())
                }

                group("Notifications", vec![
                    row(
                        "Tell me about",
                        "One choice for everything. No per-channel matrix.",
                        vec![
                            option("Everything", notify == Notify::Everything, move || app.notify.set(Notify::Everything)),
                            option("Mentions", notify == Notify::Mentions, move || app.notify.set(Notify::Mentions)),
                            option("Nothing", notify == Notify::Nothing, move || app.notify.set(Notify::Nothing)),
                        ],
                    ),
                    row(
                        "Quiet hours",
                        "Nothing buzzes between 10pm and 8am.",
                        on_off(app.quiet_hours.get(), move |v| app.quiet_hours.set(v)),
                    ),
                ])

                group("Audio & video", vec![
                    row(
                        "Join rooms muted",
                        "Walk in, listen, unmute when you have something to say.",
                        // The stored fact is "mic is on"; the question asks the
                        // opposite, so the pills invert it rather than the model
                        // carrying a second, redundant flag.
                        on_off(!app.mic.get(), move |v| app.mic.set(!v)),
                    ),
                    row(
                        "Camera on by default",
                        "Off is a perfectly normal way to be on a call.",
                        on_off(app.cam.get(), move |v| app.cam.set(v)),
                    ),
                ])

                group("Appearance & privacy", vec![
                    row(
                        "Theme",
                        "Follows your system unless you say otherwise.",
                        vec![
                            option("Light", !dark, move || app.dark.set(false)),
                            option("Dark", dark, move || app.dark.set(true)),
                        ],
                    ),
                    row(
                        "Message density",
                        "Roomy or tight. That is the extent of the customisation.",
                        vec![
                            option("Roomy", !compact, move || app.compact.set(false)),
                            option("Tight", compact, move || app.compact.set(true)),
                        ],
                    ),
                    row(
                        "Read receipts",
                        "Others see when you have read a direct message.",
                        on_off(app.read_receipts.get(), move |v| app.read_receipts.set(v)),
                    ),
                ])

                accent_group(app)
                account_group(app)

                view(style = || StyleApplication::new(styles::SettingsGroup::sheet())) {
                    view(style = || StyleApplication::new(styles::CardSection::sheet())) {
                        Strong(
                            label = "What isn't here, on purpose".to_string(),
                            kind = typography_kind::BodyLg,
                        )
                        view(style = || StyleApplication::new(styles::WrapRow::sheet())) {
                            for item in NOT_HERE {
                                Badge(
                                    label = item.to_string(),
                                    tone = tone::Primary,
                                    variant = variant::Soft,
                                )
                            }
                        }
                        Muted(
                            label = "Nothing to opt out of, because none of it was ever switched on."
                                .to_string(),
                            kind = typography_kind::BodySm,
                        )
                    }
                }
            }
        }
    }
}

/// The accent picker. Not in the design's seven — the design exposes accent as
/// a canvas prop rather than a user setting — but the theme axis exists and
/// hiding it from the person using the app would be the odd choice.
fn accent_group(app: App) -> Element {
    use crate::theme::Accent;
    let current = app.accent.get();
    group(
        "Accent",
        vec![row(
            "Colour",
            "One accent, applied everywhere. It never means anything on its own.",
            vec![
                option("Indigo", current == Accent::Indigo, move || app.accent.set(Accent::Indigo)),
                option("Teal", current == Accent::Teal, move || app.accent.set(Accent::Teal)),
                option("Amber", current == Accent::Amber, move || app.accent.set(Accent::Amber)),
                option("Violet", current == Accent::Violet, move || app.accent.set(Accent::Violet)),
            ],
        )],
    )
}

/// Who you are signed in as, and the way out.
///
/// The design has no such section — it never contemplated signing out, because
/// it never contemplated signing in. A self-hosted client that cannot leave an
/// account is a client you have to clear cookies to escape.
fn account_group(app: App) -> Element {
    let who = app
        .me
        .with(|m| m.as_ref().map(|m| format!("{} · @{}", m.display_name, m.handle)))
        .unwrap_or_default();
    group(
        "Account",
        vec![row(
            "Signed in as",
            &who,
            vec![ui! {
                Button(
                    label = "Sign out".to_string(),
                    tone = tone::Danger,
                    variant = variant::Soft,
                    size = size::Sm,
                    shape = shape::Pill,
                    on_click = cb(move || net::sign_out(app)),
                )
            }],
        )],
    )
}
