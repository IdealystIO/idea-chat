//! Live rooms — voice, video, and screen share.
//!
//! The room itself is real (it is a channel on the server); who is *in* it is
//! not. Presence needs the live socket, so this shows you and nobody else
//! rather than inventing a roster. That is deliberately visible in the UI:
//! an empty room says it is empty.

use icons_lucide::{MIC, MIC_OFF, MONITOR, MONITOR_UP, PLUS, VIDEO, VIDEO_OFF};
use idea_ui::{
    shape, size, tone, typography_kind, variant, AvatarSize, Button, Grid, Stack, StackAlign,
    StackAxis, StackGap, StackPadding, ToneRef, VariantRef,
};
use runtime_core::{component, switch, ui, Element, StyleApplication};

use crate::model::{App, ChannelKind};
use crate::net;
use crate::styles;
use crate::views::bits::{cb, press, Face, Glyph, Muted, Spring, Strong};

#[component]
pub fn LiveRoom(#[prop(static)] app: App) -> Element {
    switch(
        move || {
            (
                app.revision.get(),
                app.channel_id.get(),
                app.in_voice.get(),
                app.mic.get(),
                app.cam.get(),
                app.sharing.get(),
            )
        },
        move |_| live_body(app),
    )
}

fn live_body(app: App) -> Element {
    let body = if app.channel_kind() == ChannelKind::Share { stage(app) } else { tiles(app) };
    let bar = controls(app);

    ui! {
        view(style = || StyleApplication::new(styles::Live::sheet())) {
            body
            bar
        }
    }
}

/// The screen-share layout.
fn stage(app: App) -> Element {
    let sharing = app.sharing.get();
    let headline = if sharing {
        format!("{} is sharing", app.my_name())
    } else {
        "Nobody is sharing yet".to_string()
    };

    ui! {
        Stack(
            axis = StackAxis::Row,
            gap = StackGap::Md,
            padding = StackPadding::Lg,
            align = StackAlign::Stretch,
        ) {
            view(style = || StyleApplication::new(styles::Stage::sheet())) {
                view(style = || StyleApplication::new(styles::StageBar::sheet())) {
                    view(style = || StyleApplication::new(styles::LiveDot::sheet()))
                    Strong(label = headline, kind = typography_kind::BodySm)
                    Spring()
                }
                view(style = || StyleApplication::new(styles::StageSurface::sheet())) {
                    Glyph(data = Some(MONITOR), size = 34.0)
                    Muted(
                        label = "Screen sharing is not carried yet.".to_string(),
                        kind = typography_kind::BodySm,
                    )
                }
            }
        }
    }
}

/// The grid layout: you, and an invite slot.
fn tiles(app: App) -> Element {
    let joined = app.in_voice.get() == app.channel_id.get();
    let mut cells: Vec<Element> = Vec::new();
    if joined {
        cells.push(tile(app));
    }
    cells.push(press(
        vec![
            ui! { Glyph(data = Some(PLUS), size = 22.0) },
            ui! { Strong(label = "Invite someone".to_string(), kind = typography_kind::BodySm) },
        ],
        || StyleApplication::new(styles::TileGhost::sheet()),
        move || {
            app.minted_invite.set(String::new());
            app.join_open.set(true);
            net::mint_invite(app);
        },
    ));

    let note: Vec<Element> = if joined {
        Vec::new()
    } else {
        vec![ui! {
            Muted(
                label = "This room never ends. People come and go — nobody has to start a call."
                    .to_string(),
            )
        }]
    };

    ui! {
        scroll_view(style = || StyleApplication::new(styles::Transcript::sheet())) {
            Stack(padding = StackPadding::Xl, gap = StackGap::Lg) {
                note
                Grid(columns = 3, gap = StackGap::Md) {
                    cells
                }
            }
        }
    }
}

/// Your own tile. Everyone else's needs presence, which needs the socket.
fn tile(app: App) -> Element {
    let muted = !app.mic.get();
    let mute_mark: Vec<Element> =
        if muted { vec![ui! { Glyph(data = Some(MIC_OFF), size = 13.0) }] } else { Vec::new() };

    ui! {
        view(style = || StyleApplication::new(styles::Tile::sheet()).with("you", "on")) {
            Face(name = app.my_name(), id = app.my_id(), size = AvatarSize::Lg)
            view(style = || StyleApplication::new(styles::TileName::sheet())) {
                Strong(label = app.my_name(), kind = typography_kind::BodySm)
                mute_mark
            }
        }
    }
}

/// The floating control bar.
fn controls(app: App) -> Element {
    let joined = app.in_voice.get() == app.channel_id.get();
    let mic_on = app.mic.get();
    let cam_on = app.cam.get();
    let sharing = app.sharing.get();

    ui! {
        view(style = || StyleApplication::new(styles::CallBar::sheet())) {
            Button(
                label = if mic_on { "Mic on".to_string() } else { "Muted".to_string() },
                leading_icon = Some(if mic_on { MIC } else { MIC_OFF }),
                tone = if mic_on { ToneRef::from(tone::Neutral) } else { ToneRef::from(tone::Danger) },
                variant = variant::Soft,
                size = size::Md,
                shape = shape::Md,
                on_click = cb(move || app.mic.set(!app.mic.get())),
            )
            Button(
                label = if cam_on { "Camera on".to_string() } else { "Camera off".to_string() },
                leading_icon = Some(if cam_on { VIDEO } else { VIDEO_OFF }),
                tone = tone::Neutral,
                variant = if cam_on {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                },
                size = size::Md,
                shape = shape::Md,
                on_click = cb(move || app.cam.set(!app.cam.get())),
            )
            Button(
                label = if sharing { "Stop sharing".to_string() } else { "Share screen".to_string() },
                leading_icon = Some(MONITOR_UP),
                tone = if sharing { ToneRef::from(tone::Primary) } else { ToneRef::from(tone::Neutral) },
                variant = if sharing {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                },
                size = size::Md,
                shape = shape::Md,
                on_click = cb(move || app.sharing.set(!app.sharing.get())),
            )

            view(style = || StyleApplication::new(styles::CallBarRule::sheet()))

            Button(
                label = if joined { "Leave room".to_string() } else { "Join room".to_string() },
                tone = if joined { ToneRef::from(tone::Danger) } else { ToneRef::from(tone::Primary) },
                variant = if joined {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Filled)
                },
                size = size::Md,
                shape = shape::Md,
                on_click = cb(move || app.toggle_voice()),
            )
        }
    }
}
