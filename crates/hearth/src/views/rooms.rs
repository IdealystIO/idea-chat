//! The room list — the middle column. Channels and live rooms when a space is
//! selected, conversations when direct messages are, and the self/voice tray
//! pinned to the bottom of both.

use icons_lucide::{
    HASH, HEADPHONES, HEADPHONE_OFF, MAXIMIZE, MIC, MIC_OFF, PHONE_OFF, PLUS,
};
use idea_ui::{
    shape, size, tone, typography_kind, variant, AvatarSize, Button, IconButton, IconButtonSize,
    ToneRef, VariantRef,
};
use runtime_core::{component, icon, switch, text, ui, Element, IconData, StyleApplication};

use crate::api::types::{ChannelSummary, DmDto, MemberDto};
use crate::model::{App, ChannelKind, Scope, Status};
use crate::net;
use crate::styles;
use crate::views::bits::{cb, press, Face, Muted, Presence, SectionLabel, Spring, Strong};
use crate::views::person_menu::person_target;

/// The glyph for a channel's kind. Text channels get `#`, the one piece of
/// Discord vocabulary worth keeping — everyone already reads it.
fn channel_icon(kind: ChannelKind) -> IconData {
    match kind {
        ChannelKind::Voice => HEADPHONES,
        _ => HASH,
    }
}

#[component]
pub fn RoomList(#[prop(static)] app: App) -> Element {
    // Keyed on the loaded data and the scope — which channel is *selected* is
    // a re-style read inside each row's own style closure, not a rebuild.
    switch(
        // `in_voice` is here because the tray strip appears and disappears with
        // it — that is structural, not a re-style. Row highlighting still reads
        // it inside each row's own style closure and needs no rebuild.
        move || {
            (
                app.revision.get(),
                app.scope.get(),
                app.space_id.get(),
                app.in_voice.get(),
                app.voice_live.get(),
                app.voice_error.get(),
            )
        },
        move |_| room_list_body(app),
    )
}

fn room_list_body(app: App) -> Element {
    let in_dms = app.scope.get() == Scope::Dms;
    let title = if in_dms {
        "Direct messages".to_string()
    } else {
        app.space().map(|s| s.name).unwrap_or_default()
    };
    let meta = if in_dms {
        "Outside every space — just you and them".to_string()
    } else {
        let members = app.members.with(|m| m.len());
        match members {
            0 => String::new(),
            1 => "1 person".to_string(),
            n => format!("{n} people"),
        }
    };
    let body = if in_dms { dm_section(app) } else { space_section(app) };
    let tray = tray(app);

    ui! {
        view(style = || StyleApplication::new(styles::RoomList::sheet())) {
            view(style = || StyleApplication::new(styles::RoomListHead::sheet())) {
                Strong(label = title, kind = typography_kind::BodyLg)
                Muted(label = meta, kind = typography_kind::Caption)
            }
            scroll_view(style = || StyleApplication::new(styles::RoomListBody::sheet())) {
                body
            }
            tray
        }
    }
}

/// Channels, then live rooms.
fn space_section(app: App) -> Element {
    let channels: Vec<Element> =
        app.channels_of_kind(true).into_iter().map(|c| channel_row(app, c)).collect();
    let rooms: Vec<Element> =
        app.channels_of_kind(false).into_iter().map(|c| live_room_row(app, c)).collect();
    let empty: Vec<Element> = if channels.is_empty() && rooms.is_empty() {
        // A space with nothing in it is a real state — a moderator has to be
        // able to tell "still loading" from "nothing here yet".
        vec![ui! {
            view(style = || StyleApplication::new(styles::RoomListPad::sheet())) {
                Muted(label = "No channels yet.".to_string(), kind = typography_kind::BodySm)
            }
        }]
    } else {
        Vec::new()
    };

    ui! {
        view(style = || StyleApplication::new(styles::RoomListPad::sheet())) {
            SectionLabel(label = "Talk".to_string())
            channels
            SectionLabel(label = "Rooms".to_string())
            rooms
            empty
        }
    }
}

/// A text channel in the list.
///
/// Selection is read inside the style sinks, not passed in: moving between
/// channels is a re-tint of two rows, and expressing it as a rebuild of the
/// whole list would be both slower and — because a rebuild discards per-row
/// state — wrong.
fn channel_row(app: App, channel: ChannelSummary) -> Element {
    let id = channel.id.clone();
    let for_style = id.clone();
    let for_pip = id.clone();
    let active = move || app.channel_id.get() == for_style && !app.settings_open.get();

    let label: Element = text(channel.name.clone())
        .with_style(|| StyleApplication::new(styles::RoomRowName::sheet()))
        .into();
    let pip: Element = runtime_core::view(Vec::new())
        .with_style(move || {
            StyleApplication::new(styles::UnreadDot::sheet())
                .with("shown", if app.is_unread(&for_pip) { "on" } else { "off" })
        })
        .into();

    press(
        vec![icon(HASH).size(15.0).into(), label, pip],
        move || {
            StyleApplication::new(styles::RoomRow::sheet())
                .with("active", if active() { "on" } else { "off" })
        },
        move || net::go_channel(app, &id),
    )
}

/// A voice room. Clicking it connects you, there and then.
///
/// It deliberately does not navigate. Joining a call is not a reason to stop
/// reading what you were reading — the tray at the bottom becomes the whole
/// interface for being in a room, and the call view is a second, optional act
/// (click the room again once connected).
///
/// The occupant list under each room is not wired yet — presence needs the
/// live socket, and inventing a roster from nothing would be a lie the UI
/// tells convincingly.
fn live_room_row(app: App, channel: ChannelSummary) -> Element {
    let id = channel.id.clone();
    let for_style = id.clone();
    let for_click = id.clone();
    let kind = ChannelKind::parse(&channel.kind);
    // Highlighted because you are IN it, not because it is selected — a voice
    // room is never the selected channel.
    let active = move || app.in_voice.get() == for_style;

    let label: Element = text(channel.name.clone())
        .with_style(|| StyleApplication::new(styles::RoomRowName::sheet()))
        .into();
    let mut children = vec![icon(channel_icon(kind)).size(15.0).into(), label];
    if app.in_voice.get() == id {
        children.push(ui! { Muted(label = "1".to_string(), kind = typography_kind::Caption) });
    }

    press(
        children,
        move || {
            StyleApplication::new(styles::RoomRow::sheet())
                .with("active", if active() { "on" } else { "off" })
        },
        move || {
            // Already in it? Then this click means "show me the call".
            if app.in_voice.get() == for_click {
                app.open_stage(&for_click);
            } else {
                app.join_voice(&for_click);
            }
        },
    )
}

/// Direct messages: avatar, name, and the last line.
fn dm_section(app: App) -> Element {
    let rows: Vec<Element> = app.dms.with(|dms| dms.clone()).into_iter().map(|d| dm_row(app, d)).collect();
    let empty: Vec<Element> = if rows.is_empty() {
        vec![ui! {
            view(style = || StyleApplication::new(styles::RoomListPad::sheet())) {
                Muted(
                    label = "No conversations yet. Open someone's profile to start one."
                        .to_string(),
                    kind = typography_kind::BodySm,
                )
            }
        }]
    } else {
        Vec::new()
    };

    ui! {
        view(style = || StyleApplication::new(styles::RoomListPad::sheet())) {
            SectionLabel(label = "Conversations".to_string())
            rows
            empty
        }
    }
}

fn dm_row(app: App, dm: DmDto) -> Element {
    let id = dm.channel_id.clone();
    let for_style = id.clone();
    let for_pip = id.clone();
    let active = move || app.channel_id.get() == for_style && !app.settings_open.get();

    let name: Element = text(dm.other_name.clone())
        .with_style(|| StyleApplication::new(styles::RoomRowName::sheet()))
        .into();
    let summary: Element = ui! {
        view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
            name
            Muted(label = dm.preview.clone(), kind = typography_kind::BodySm)
        }
    };
    let pip: Element = runtime_core::view(Vec::new())
        .with_style(move || {
            StyleApplication::new(styles::UnreadDot::sheet())
                .with("shown", if app.is_unread(&for_pip) { "on" } else { "off" })
        })
        .into();

    press(
        vec![
            ui! {
                Presence(
                    name = dm.other_name.clone(),
                    id = dm.other_id.clone().unwrap_or_default(),
                    size = AvatarSize::Sm,
                    status = Status::Online,
                    small = false,
                )
            },
            summary,
            pip,
        ],
        move || {
            StyleApplication::new(styles::DmRow::sheet())
                .with("active", if active() { "on" } else { "off" })
        },
        move || net::go_channel(app, &id),
    )
}

// ===========================================================================
// The tray
// ===========================================================================

/// The bottom of the room list: the room you're in, then you and your mic.
fn tray(app: App) -> Element {
    let in_voice = app.in_voice.get();
    let deafened = app.deafened.get();
    let mic_on = app.mic.get();

    // The strip IS the call interface. Everything you routinely do while in a
    // room — see that you are connected, open the call, leave — is here, so
    // that joining never has to take over the main column.
    let strip: Vec<Element> = if in_voice.is_empty() {
        Vec::new()
    } else {
        let room_name = app.voice_room_name();
        let open_room = in_voice.clone();
        let live = app.voice_live.get();
        let trouble = app.voice_error.get();
        // Plain: `Ellipsis` on a text node zeroes its height, because its
        // `flex_basis: 0` applies on the cross axis in a column and the line
        // box collapses. The wrapper does the clipping instead.
        let name_node: Element = text(room_name).into();
        // Says what is true. "Connected" while the microphone is still being
        // opened is the kind of small lie that makes people talk into a call
        // that is not carrying them yet.
        let status: Element = if !trouble.is_empty() {
            ui! { Strong(label = trouble, kind = typography_kind::Overline) }
        } else if live {
            ui! { Muted(label = "Voice connected".to_string(), kind = typography_kind::Overline) }
        } else {
            ui! { Muted(label = "Connecting…".to_string(), kind = typography_kind::Overline) }
        };
        vec![ui! {
            view(style = || StyleApplication::new(styles::VoiceStrip::sheet())) {
                view(style = move || {
                    StyleApplication::new(styles::Meter::sheet())
                        .with("live", if live { "on" } else { "off" })
                }) {
                    view(style = || StyleApplication::new(styles::LevelBar::sheet()).with("step", "a"))
                    view(style = || StyleApplication::new(styles::LevelBar::sheet()).with("step", "b"))
                    view(style = || StyleApplication::new(styles::LevelBar::sheet()).with("step", "c"))
                }
                view(style = || StyleApplication::new(styles::VoiceStripText::sheet())) {
                    status
                    name_node
                }
                IconButton(
                    glyph = String::new(),
                    icon = Some(MAXIMIZE),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Sm,
                    on_click = cb(move || app.open_stage(&open_room)),
                )
                IconButton(
                    glyph = String::new(),
                    icon = Some(PHONE_OFF),
                    tone = tone::Danger,
                    variant = variant::Ghost,
                    size = IconButtonSize::Sm,
                    on_click = cb(move || app.leave_voice()),
                )
            }
        }]
    };

    let status = if deafened {
        "Sound off"
    } else if mic_on {
        "Mic live"
    } else {
        "Muted"
    };

    ui! {
        view(style = || StyleApplication::new(styles::Tray::sheet())) {
            strip
            view(style = || StyleApplication::new(styles::TrayRow::sheet())) {
                Face(name = app.my_name(), id = app.my_id(), size = AvatarSize::Sm)
                view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
                    Strong(label = app.my_name(), kind = typography_kind::BodySm)
                    Muted(label = status.to_string(), kind = typography_kind::Overline)
                }
                IconButton(
                    glyph = String::new(),
                    icon = Some(if mic_on { MIC } else { MIC_OFF }),
                    tone = if mic_on { ToneRef::from(tone::Neutral) } else { ToneRef::from(tone::Danger) },
                    variant = if mic_on {
                        VariantRef::from(variant::Ghost)
                    } else {
                        VariantRef::from(variant::Soft)
                    },
                    size = IconButtonSize::Sm,
                    on_click = cb(move || app.mic.set(!app.mic.get())),
                )
                IconButton(
                    glyph = String::new(),
                    icon = Some(if deafened { HEADPHONE_OFF } else { HEADPHONES }),
                    tone = if deafened { ToneRef::from(tone::Danger) } else { ToneRef::from(tone::Neutral) },
                    variant = if deafened {
                        VariantRef::from(variant::Soft)
                    } else {
                        VariantRef::from(variant::Ghost)
                    },
                    size = IconButtonSize::Sm,
                    on_click = cb(move || app.toggle_deafened()),
                )
            }
        }
    }
}

// ===========================================================================
// People sidebar
// ===========================================================================

/// Who is in this space.
#[component]
pub fn PeopleSidebar(#[prop(static)] app: App) -> Element {
    switch(move || (app.revision.get(), app.menu.get()), move |_| people_body(app))
}

fn people_body(app: App) -> Element {
    let members = app.members.with(|m| m.clone());
    let label = format!("In this space — {}", members.len());
    let rows: Vec<Element> = members.into_iter().map(|m| person_row(app, m)).collect();
    let invite: Vec<Element> = if app.moderates() {
        vec![ui! {
            Button(
                label = "Invite someone".to_string(),
                leading_icon = Some(PLUS),
                tone = tone::Neutral,
                variant = variant::Soft,
                size = size::Sm,
                shape = shape::Md,
                on_click = cb(move || {
                    app.minted_invite.set(String::new());
                    app.join_open.set(true);
                    net::mint_invite(app);
                }),
            )
        }]
    } else {
        Vec::new()
    };

    ui! {
        view(style = || StyleApplication::new(styles::People::sheet())) {
            scroll_view(style = || StyleApplication::new(styles::RoomListBody::sheet())) {
                view(style = || StyleApplication::new(styles::PeoplePad::sheet())) {
                    SectionLabel(label = label)
                    rows
                    Spring()
                    invite
                }
            }
        }
    }
}

fn person_row(app: App, member: MemberDto) -> Element {
    let note = if app.is_muted(&member.id) {
        "Muted for you".to_string()
    } else if member.role == "member" {
        String::new()
    } else {
        // Owner and moderator are worth showing; "member" is the default and
        // labelling everyone with it is noise.
        let mut r = member.role.clone();
        r.get_mut(0..1).map(|c| c.make_ascii_uppercase());
        r
    };

    let member_id = member.id.clone();
    person_target(app, "roster", &member_id, move |open| {
        let name: Element = ui! {
            Strong(label = member.display_name.clone(), kind = typography_kind::BodySm)
        };
        let note_node: Vec<Element> = if note.is_empty() {
            Vec::new()
        } else {
            vec![ui! { Muted(label = note.clone(), kind = typography_kind::Overline) }]
        };
        let block: Element = ui! {
            view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
                name
                note_node
            }
        };
        press(
            vec![
                ui! {
                    Presence(
                        name = member.display_name.clone(),
                        id = member.id.clone(),
                        size = AvatarSize::Xs,
                        status = Status::Online,
                        small = true,
                    )
                },
                block,
            ],
            move || StyleApplication::new(styles::PersonRow::sheet()).with("present", "here"),
            open,
        )
    })
}

/// Whether the people sidebar should be showing at all.
pub fn people_visible(app: App) -> bool {
    app.scope.get() == Scope::Space && app.show_people.get()
}
