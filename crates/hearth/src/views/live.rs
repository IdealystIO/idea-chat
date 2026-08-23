//! Live rooms — voice, video, and screen share.
//!
//! One view for all three. A room is a room; the only differences are whether
//! there's a stage to put someone's screen on and whether cameras are expected.
//! That is the design's point: nobody "starts a call", they walk in.

use icons_lucide::{MIC, MIC_OFF, MONITOR, MONITOR_UP, PLUS, VIDEO, VIDEO_OFF};
use idea_ui::{shape, size, tone, typography_kind, variant, AvatarSize, Button, Grid, Stack, StackAlign, StackAxis, StackGap, StackPadding, ToneRef, VariantRef};
use runtime_core::{switch, component, icon, ui, Element, StyleApplication};

use crate::model::{App, Room, RoomKind};
use crate::styles;
use crate::views::bits::{cb, press, Face, Glyph, Muted, Spring, Strong};
use crate::views::person_menu::person_target;

/// Who the design shows as actively speaking.
///
/// A real client reads this off the audio path — `voice-client` already
/// computes exactly this signal from its mic/remote peak meters — so this is
/// the seam that pipeline plugs into, not a decoration.
fn is_speaking(name: &str) -> bool {
    name == "Ana Ruiz" || name == "Mom"
}

/// Who is muted. Same note as [`is_speaking`]: server state, stubbed.
fn is_muted_in_room(app: App, name: &str) -> bool {
    if name == "You" {
        return !app.mic.get();
    }
    name == "Theo Lang" || name == "Dad" || app.is_timed_out(name)
}

#[component]
pub fn LiveRoom(#[prop(static)] app: App) -> Element {
    switch(move || (app.nav_key(), app.voice_key(), app.people_key()), move |_| live_body(app))
}

fn live_body(app: App) -> Element {
    let Some(room) = app.current_room() else {
        return ui! { view() };
    };
    let body = if room.kind == RoomKind::Share { stage(app, room) } else { tiles(app, room) };
    let bar = controls(app, room);

    ui! {
        view(style = || StyleApplication::new(styles::Live::sheet())) {
            body
            bar
        }
    }
}

/// The screen-share layout: one big stage, a strip of faces beside it.
fn stage(app: App, room: &'static Room) -> Element {
    let sharer = room.people.first().copied().unwrap_or("Nobody");
    let strip: Vec<Element> =
        app.occupants(room).into_iter().map(|name| tile(app, room, name, true)).collect();

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
                    Strong(label = format!("{sharer} is sharing"), kind = typography_kind::BodySm)
                    Spring()
                    Muted(label = "1080p · 22 min".to_string(), kind = typography_kind::Overline)
                }
                view(style = || StyleApplication::new(styles::StageSurface::sheet())) {
                    Glyph(data = Some(MONITOR), size = 34.0)
                    Muted(label = "Shared screen".to_string(), kind = typography_kind::BodySm)
                }
            }
            Stack(gap = StackGap::Sm) {
                strip
            }
        }
    }
}

/// The grid layout: every face the same size, plus an invite slot.
fn tiles(app: App, room: &'static Room) -> Element {
    let mut cells: Vec<Element> =
        app.occupants(room).into_iter().map(|name| tile(app, room, name, false)).collect();

    cells.push(press(
        vec![
            icon(PLUS).size(22.0).into(),
            ui! {
                Strong(label = "Invite someone".to_string(), kind = typography_kind::BodySm)
            },
        ],
        || StyleApplication::new(styles::TileGhost::sheet()),
        move || app.join_open.set(true),
    ));

    // Three columns. `Grid` takes a fixed count rather than a min width, so
    // this is the density that keeps a face legible on a laptop without
    // stranding one tile alone on a row in the common 2–4 person case.
    ui! {
        scroll_view(style = || StyleApplication::new(styles::Transcript::sheet())) {
            Stack(padding = StackPadding::Xl) {
                Grid(columns = 3, gap = StackGap::Md) {
                    cells
                }
            }
        }
    }
}

/// One person's tile. `narrow` is the screen-share filmstrip variant.
fn tile(app: App, room: &'static Room, name: String, narrow: bool) -> Element {
    let you = name == "You";
    let speaking = is_speaking(&name);
    let quiet = is_muted_in_room(app, &name);
    let size = if narrow { AvatarSize::Md } else { AvatarSize::Lg };
    let site = format!("tile:{}", room.id);

    let owned = name.clone();
    person_target(app, &site, &name, move |open| {
        let plate_name = owned.clone();
        let mute_mark: Vec<Element> =
            if quiet { vec![icon(MIC_OFF).size(13.0).into()] } else { Vec::new() };
        let plate: Element = ui! {
            view(style = || StyleApplication::new(styles::TileName::sheet())) {
                Strong(label = plate_name, kind = typography_kind::BodySm)
                mute_mark
            }
        };
        press(
            vec![ui! { Face(name = owned.clone(), size = size) }, plate],
            move || {
                StyleApplication::new(styles::Tile::sheet())
                    .with("speaking", if speaking { "on" } else { "off" })
                    .with("you", if you { "on" } else { "off" })
            },
            open,
        )
    })
}

/// The floating control bar: mic, camera, share, and join/leave.
fn controls(app: App, room: &'static Room) -> Element {
    let joined = app.in_voice.get() == room.id;
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
