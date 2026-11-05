//! The call view — what a voice room looks like when you ask to see it.
//!
//! This is not where you go to join a room; clicking a room in the list
//! already did that, and the tray is the interface for being in one. This view
//! is the optional conference surface, opened deliberately and closed back to
//! whatever you were reading. Camera and screen share are controls in here,
//! not separate kinds of room.
//!
//! Everyone in the room gets a tile, from presence: their camera when it is
//! on, otherwise their avatar; a screen share gets a tile of its own, first.
//! The tiles rebuild when someone arrives or leaves, or a camera or a share
//! starts or stops — never for a mute, which only redraws its own mark,
//! because a rebuild restarts every video on the stage.

use icons_lucide::{MIC, MIC_OFF, MONITOR_UP, PHONE_OFF, PLUS, VIDEO, VIDEO_OFF};
use idea_ui::{
    shape, size, tone, typography_kind, variant, AvatarSize, Button, Grid, Stack, StackGap,
    StackPadding, ToneRef, VariantRef,
};
use runtime_core::{component, switch, ui, Element, StyleApplication};

use crate::model::VideoState;

use crate::model::App;
use crate::net;
use crate::styles;
use crate::views::bits::{cb, press, Face, Glyph, Muted, Strong};

#[component]
pub fn LiveRoom(#[prop(static)] app: App) -> Element {
    // Two regions on different keys: the tiles on who is here and what video
    // there is, the bar on the buttons. A mic toggle must not rebuild tiles.
    let body = switch(
        move || (app.revision.get(), app.stage.get(), app.in_voice.get(), app.presence_key(), app.video.get()),
        move |_| tiles(app),
    );
    let bar = switch(
        move || (app.stage.get(), app.in_voice.get(), app.mic.get(), app.cam.get(), app.sharing.get()),
        move |_| controls(app),
    );

    ui! {
        view(style = || StyleApplication::new(styles::Live::sheet())) {
            body
            bar
        }
    }
}

/// The grid: screen shares, then everyone in the room, then an invite slot.
fn tiles(app: App) -> Element {
    let joined = !app.in_voice.get().is_empty() && app.in_voice.get() == app.stage.get();
    let me = app.my_id();
    let video = app.video.get();
    let mut cells: Vec<Element> = Vec::new();
    if joined {
        // Shares first: usually what everyone is looking at.
        if video.screen {
            cells.push(screen_tile(app, &me, "Your screen".to_string()));
        }
        let people = app.people_in_room(&app.stage.get());
        for (user, _) in video.remote.iter().filter(|(_, t)| *t == call_proto::TRACK_SCREEN) {
            let name = people.iter().find(|p| &p.id == user).map(|p| p.display_name.clone()).unwrap_or_default();
            cells.push(screen_tile(app, user, format!("{name}'s screen")));
        }
        // You first, then the rest — from presence, which includes us once the
        // join has been announced; until then, us alone.
        cells.push(person_tile(app, &me, app.my_name(), &video));
        for p in people.into_iter().filter(|p| p.id != me) {
            cells.push(person_tile(app, &p.id, p.display_name.clone(), &video));
        }
    }
    cells.push(press(
        vec![
            ui! { Glyph(data = Some(PLUS), size = 22.0) },
            ui! { Strong(label = "Invite someone".to_string(), kind = typography_kind::BodySm) },
        ],
        || StyleApplication::new(styles::TileGhost::sheet()),
        move || {
            app.minted_invite.set(String::new());
            app.invite_copied.set(false);
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

/// Someone's tile: their camera when it is on, else their face; their name,
/// a mic-off mark while they are muted, and the speaking ring.
fn person_tile(app: App, user: &str, name: String, video: &VideoState) -> Element {
    let you = user == app.my_id();
    let camera_on = if you { video.camera } else { video.remote.iter().any(|(u, t)| u == user && *t == call_proto::TRACK_MAIN) };
    let picture = if camera_on { player(app, user, call_proto::TRACK_MAIN) } else { None };
    let over_video = picture.is_some();
    let body: Element = picture.unwrap_or_else(|| ui! { Face(name = name.clone(), id = user.to_string(), size = AvatarSize::Lg) });

    // Live, so a mute redraws the mark and nothing else.
    let mark_user = user.to_string();
    let mark = switch(
        move || if you { !app.mic.get() } else { app.mic_off.get().contains(&mark_user) },
        |&muted: &bool| {
            if muted {
                ui! { Glyph(data = Some(MIC_OFF), size = 13.0) }
            } else {
                runtime_core::fragment(Vec::new())
            }
        },
    );
    let label: Element = ui! { Strong(label = name, kind = typography_kind::BodySm) };
    let plate: Element = if over_video {
        ui! { view(style = || StyleApplication::new(styles::TilePlate::sheet())) { label } }
    } else {
        label
    };
    let speaker = user.to_string();
    runtime_core::view(vec![
        body,
        ui! {
            view(style = || StyleApplication::new(styles::TileName::sheet())) {
                plate
                mark
            }
        },
    ])
    .with_style(move || {
        let speaking = if you { app.speaking.get() } else { app.speaking_now.get().contains(&speaker) };
        StyleApplication::new(styles::Tile::sheet())
            .with("you", if you { "on" } else { "off" })
            .with("speaking", if speaking { "on" } else { "off" })
    })
    .into()
}

/// A screen share's tile.
fn screen_tile(app: App, user: &str, label: String) -> Element {
    let body = player(app, user, call_proto::TRACK_SCREEN).unwrap_or_else(|| runtime_core::fragment(Vec::new()));
    ui! {
        view(style = || StyleApplication::new(styles::Tile::sheet())) {
            body
            view(style = || StyleApplication::new(styles::TileName::sheet())) {
                view(style = || StyleApplication::new(styles::TilePlate::sheet())) {
                    Strong(label = label, kind = typography_kind::BodySm)
                }
            }
        }
    }
}

/// A video to show, where the call has it.
fn player(app: App, user: &str, track: u8) -> Option<Element> {
    #[cfg(target_arch = "wasm32")]
    {
        crate::voice::player(app, user, track)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (app, user, track);
        None
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
                on_click = cb(move || {
                    // The value, not a re-read: `Signal::set` STAGES, so asking
                    // `app.mic` here returns the one being replaced and the
                    // microphone would be set to the opposite of the button.
                    let on = !app.mic.get();
                    app.mic.set(on);
                    crate::set_muted(!on);
                }),
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
