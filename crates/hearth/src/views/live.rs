//! The call view — what a voice room looks like when you ask to see it.
//!
//! This is not where you go to join a room; clicking a room in the list
//! already did that, and the tray is the interface for being in one. This view
//! is the optional conference surface, opened deliberately and closed back to
//! whatever you were reading. Camera and screen share are controls in here,
//! not separate kinds of room.
//!
//! The room itself is real (it is a channel on the server); who is *in* it is
//! not. Presence needs the live socket, so this shows you and nobody else
//! rather than inventing a roster. That is deliberately visible in the UI:
//! an empty room says it is empty.

use icons_lucide::{MIC, MIC_OFF, MONITOR_UP, PHONE_OFF, PLUS, VIDEO, VIDEO_OFF};
use idea_ui::{
    shape, size, tone, typography_kind, variant, AvatarSize, Button, Grid, Stack, StackGap,
    StackPadding, ToneRef, VariantRef,
};
use runtime_core::{component, switch, ui, Element, StyleApplication};

use crate::model::App;
use crate::net;
use crate::styles;
use crate::views::bits::{cb, press, Face, Glyph, Muted, Strong};

#[component]
pub fn LiveRoom(#[prop(static)] app: App) -> Element {
    switch(
        move || {
            (
                app.revision.get(),
                app.stage.get(),
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
    let body = tiles(app);
    let bar = controls(app);

    ui! {
        view(style = || StyleApplication::new(styles::Live::sheet())) {
            body
            bar
        }
    }
}

/// The grid layout: you, and an invite slot.
fn tiles(app: App) -> Element {
    let joined = !app.in_voice.get().is_empty() && app.in_voice.get() == app.stage.get();
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
    let joined = !app.in_voice.get().is_empty() && app.in_voice.get() == app.stage.get();
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

            // Two different exits, and they are not the same thing: one puts
            // the main column back where it was and keeps you in the call, the
            // other hangs up. Conflating them is how people leave a call by
            // accident when they only wanted to read something.
            Button(
                label = "Back to chat".to_string(),
                tone = tone::Neutral,
                variant = variant::Ghost,
                size = size::Md,
                shape = shape::Md,
                on_click = cb(move || app.close_stage()),
            )
            Button(
                label = "Leave room".to_string(),
                leading_icon = Some(PHONE_OFF),
                tone = tone::Danger,
                variant = if joined {
                    VariantRef::from(variant::Soft)
                } else {
                    VariantRef::from(variant::Ghost)
                },
                size = size::Md,
                shape = shape::Md,
                disabled = !joined,
                on_click = cb(move || app.leave_voice()),
            )
        }
    }
}
