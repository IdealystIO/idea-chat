//! Settings. A short list, and a longer list of what deliberately isn't here.
//!
//! Each row is a question with two or three answers, rendered as a pill group
//! rather than a switch: the options are the documentation. A row that reads
//! "Tell me about — Everything / Mentions / Nothing" needs no help text to
//! explain what the middle setting does.

use icons_lucide::{VOLUME_2, VOLUME_X};
use idea_ui::{shape, size, tone, typography_kind, variant, Badge, Button, ControlSize, Slider, Stack, StackAxis, StackGap, ToneRef, Typography, VariantRef};
use runtime_core::{switch, component, text, ui, Element, StyleApplication};

use crate::model::{App, Notify};
use voice_dsp::{Mode, Strength};
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
    // Nested rather than flat: `PartialEq` on tuples stops at twelve elements,
    // and this page has more settings than that now. Grouping the suppression
    // ones is also just true — they are one subject.
    switch(
        move || {
            (
                app.dark.get(),
                app.accent.get(),
                app.notify.get(),
                app.quiet_hours.get(),
                app.compact.get(),
                app.read_receipts.get(),
                app.mic.get(),
                app.cam.get(),
                (
                    app.suppress.get(),
                    app.in_voice.get(),
                    app.sup_transients.get(),
                    app.sup_residual.get(),
                    app.sup_leveling.get(),
                ),
            )
        },
        move |_| settings_body(app),
    )
}

fn settings_body(app: App) -> Element {
    let notify = app.notify.get();
    let suppress = app.suppress.get();
    let compact = app.compact.get();
    let dark = app.dark.get();

    ui! {
        scroll_view(style = || StyleApplication::new(styles::SettingsPage::sheet())) {
            view(style = || StyleApplication::new(styles::SettingsColumn::sheet())) {
                view(style = || StyleApplication::new(styles::Column::sheet())) {
                    Typography(content = "Settings".to_string(), kind = typography_kind::H1)
                    Muted(label = "A short list. That's the whole list.".to_string())
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
                    row(
                        "How loud everyone is",
                        // Says what the number is FOR, because the obvious
                        // reading — "why would I not leave this at 100" — is
                        // the one that makes the per-person control useless.
                        "Only on this device, and nobody is told. Leaving room below full is what lets you turn one person up in their own menu.",
                        vec![master_volume(app)],
                    ),
                    row(
                        "Noise suppression",
                        // Says what each choice costs, because the choice is
                        // not "better or worse" — it is who pays.
                        "Neural, on top of your browser's. On this device costs an 8 MB download and some CPU; on the server costs neither, but the noise still travels.",
                        vec![
                            option("Off", suppress == Mode::Off, move || pick(app, Mode::Off)),
                            option("This device", suppress == Mode::Client, move || pick(app, Mode::Client)),
                            option("Server", suppress == Mode::Server, move || pick(app, Mode::Server)),
                        ],
                    ),
                    suppression_note(app),
                ])

                suppression_stages(app)

                group("Appearance & privacy", vec![
                    row(
                        "Theme",
                        "Follows your system unless you say otherwise.",
                        vec![
                            option("Light", !dark, move || {
                                app.dark.set(false);
                                crate::prefs::set_flag("dark", false);
                            }),
                            option("Dark", dark, move || {
                                app.dark.set(true);
                                crate::prefs::set_flag("dark", true);
                            }),
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

/// The stages, each switchable on its own.
///
/// Hidden entirely when suppression is off, because every row here describes
/// something that only happens when there is a model in the chain — and a
/// settings page that shows controls which do nothing is a settings page people
/// stop reading.
///
/// Independently switchable is the point rather than a nicety. When a call
/// sounds wrong the question is always *which stage*, and the only reliable way
/// to answer it is to turn them off one at a time and listen.
fn suppression_stages(app: App) -> Vec<Element> {
    if app.suppress.get() == Mode::Off {
        return Vec::new();
    }
    let transients = app.sup_transients.get();
    let residual = app.sup_residual.get();

    vec![group("Suppression stages", vec![
        row(
            "Keystrokes and knocks",
            "Impulsive noise the model leaves behind. Stronger also softens hard consonants.",
            strengths(transients, move |s| {
                app.sup_transients.set(s);
                crate::prefs::set("suppress.transients", s.as_str());
                restage(app);
            }),
        ),
        row(
            "Residual hiss",
            "What survives the model. Driven by the model's own verdict on each block, so it can tell a quiet word from a quiet noise.",
            strengths(residual, move |s| {
                app.sup_residual.set(s);
                crate::prefs::set("suppress.residual", s.as_str());
                restage(app);
            }),
        ),
        row(
            "Even out volume",
            "Brings everyone to a similar level, so nobody has to ride the slider.",
            on_off(app.sup_leveling.get(), move |v| {
                app.sup_leveling.set(v);
                crate::prefs::set_flag("suppress.leveling", v);
                restage(app);
            }),
        ),
    ])]
}

/// The four-way strength picker, shared by every stage that has one.
fn strengths(current: Strength, set: impl Fn(Strength) + 'static) -> Vec<Element> {
    let set = std::rc::Rc::new(set);
    [Strength::Off, Strength::Light, Strength::Standard, Strength::Strong]
        .into_iter()
        .map(|s| {
            let set = set.clone();
            option(s.label(), current == s, move || set(s))
        })
        .collect()
}

/// Rebuild the capture chain so a stage change is audible now rather than on
/// the next call.
///
/// The tuning is read when the chain is built, so this is the same swap the
/// mode picker performs — and it goes through the same lazily-loaded chunk,
/// which is why it is a call into `voice` rather than anything done here.
fn restage(app: App) {
    #[cfg(target_arch = "wasm32")]
    crate::voice::set_suppression(app, app.suppress.get());
    #[cfg(not(target_arch = "wasm32"))]
    let _ = app;
}

/// Choose where suppression runs, and apply it to the call in progress.
///
/// Two writes rather than one, because the setting and the running call are
/// genuinely two things: the signal is what the next join will read, and
/// `set_suppression` is what makes the current one change. Passing the mode
/// explicitly matters — `Signal::set` stages, so reading it back on the next
/// line would still give the old value.
fn pick(app: App, mode: Mode) {
    app.suppress.set(mode);
    crate::prefs::set("suppress", mode.as_str());
    #[cfg(target_arch = "wasm32")]
    crate::voice::set_suppression(app, mode);
}

/// What the current choice actually means, right now.
///
/// Three different things need saying and only one of them at a time, which is
/// why this is a row rather than help text on the control: the setting is a
/// *request*, and the answer depends on the relay, on whether a model has been
/// fetched, and on whether a call is even up. A static caption could only ever
/// describe one of those.
fn suppression_note(app: App) -> Element {
    let text = match app.suppress.get() {
        Mode::Off => {
            "Your browser's own suppression is doing the work — the same as any other video call."
        }
        Mode::Client => {
            "DeepFilterNet runs here, before anything is sent. The first call after choosing this downloads the model, so it connects more slowly than the ones after it."
        }
        Mode::Server => {
            "DeepFilterNet runs on the relay. Nothing to download — but the relay has to be willing, and it says so when you connect. If it declines you get an ordinary call, not a failed one."
        }
    };
    // Only meaningful once there is a call to describe — and only the server
    // choice has to wait. The client chain is swapped under the running call
    // (see `voice::set_suppression`); the node's decision is made when the
    // relay socket opens and cannot be revisited without opening another.
    let live = if app.in_voice.get().is_empty() || app.suppress.get() != Mode::Server {
        String::new()
    } else {
        " Takes effect on your next call.".to_string()
    };
    ui! {
        view(style = || StyleApplication::new(styles::SettingsRow::sheet())) {
            Muted(label = format!("{text}{live}"), kind = typography_kind::BodySm)
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

/// The master volume slider.
///
/// Not `option` pills like the rest of this page, because loudness is the one
/// setting here that is genuinely continuous — three named steps would be
/// three guesses at somebody else's headphones.
///
/// Deliberately absent from the `switch` key at the top of this page: a slider
/// that re-keys on its own value is rebuilt mid-drag, which destroys the view
/// holding the pointer and stalls the drag after the first move. The `Slider`
/// patches its fill in place from the signal instead.
fn master_volume(app: App) -> Element {
    let on_change = std::rc::Rc::new(move |position: f32| {
        // The signal first, so the slider tracks the drag; then the store, so
        // it survives a refresh; then the call, so it is audible now.
        app.volume.set(position);
        crate::volume::set_master(position);
        #[cfg(target_arch = "wasm32")]
        crate::voice::refresh_master(position);
    }) as std::rc::Rc<dyn Fn(f32)>;

    ui! {
        view(style = || StyleApplication::new(styles::Row::sheet())) {
            Slider(
                value = app.volume,
                on_change = on_change,
                min = 0.0,
                max = 1.0,
                size = ControlSize::Sm,
                leading_icon = Some(VOLUME_X),
                trailing_icon = Some(VOLUME_2),
            )
            text(move || format!("{:.0}%", app.volume.get() * 100.0))
                .with_style(|| StyleApplication::new(styles::VolumeReadout::sheet()))
        }
    }
}
