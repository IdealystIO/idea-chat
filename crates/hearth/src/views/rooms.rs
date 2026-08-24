//! The room list — the middle column. Channels and live rooms when a space is
//! selected, conversations when direct messages are, and the self/voice tray
//! pinned to the bottom of both.

use icons_lucide::{
    HASH, HEADPHONES, HEADPHONE_OFF, MIC, MIC_OFF, MONITOR_UP, PLUS, VIDEO,
};
use idea_ui::{shape, size, tone, typography_kind, variant, AvatarSize, Button, IconButton, IconButtonSize, ToneRef, VariantRef};
use runtime_core::{switch, component, icon, text, ui, Element, IconData, StyleApplication};

use crate::model::{self, App, Person, RoomKind, Scope, Status, DMS};
use crate::styles;
use crate::views::bits::{cb, press, Face, Muted, Presence, SectionLabel, Spring, Strong};
use crate::views::person_menu::person_target;

/// The glyph for a room's kind. Text channels get `#`, the one piece of
/// Discord vocabulary worth keeping — everyone already reads it.
fn room_icon(kind: RoomKind) -> IconData {
    match kind {
        RoomKind::Text => HASH,
        RoomKind::Voice => HEADPHONES,
        RoomKind::Video => VIDEO,
        RoomKind::Share => MONITOR_UP,
    }
}

#[component]
pub fn RoomList(#[prop(static)] app: App) -> Element {
    switch(move || (app.nav_key(), app.voice_key(), app.people_key(), app.content_key()), move |_| room_list_body(app))
}

fn room_list_body(app: App) -> Element {
    let in_dms = app.scope.get() == Scope::Dms;
    let space = model::space(app.space_id.get());
    let title = if in_dms { "Direct messages".to_string() } else { space.name.to_string() };
    let meta = if in_dms {
        "Outside every space — just you and them".to_string()
    } else {
        space.meta.to_string()
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

/// Channels, then live rooms with their occupants nested underneath.
fn space_section(app: App) -> Element {
    let space_id = app.space_id.get();

    ui! {
        view(style = || StyleApplication::new(styles::RoomListPad::sheet())) {
            SectionLabel(label = "Talk".to_string())
            for room in model::rooms_in(space_id).filter(|r| r.kind.is_chat()), key = room.id {
                channel_row(app, room)
            }
            SectionLabel(label = "Rooms".to_string())
            for room in model::rooms_in(space_id).filter(|r| !r.kind.is_chat()), key = room.id {
                live_room_row(app, room)
            }
        }
    }
}

/// A text channel in the list.
///
/// Selection is read inside the style sinks, not passed in: moving between
/// channels is a re-tint of two rows, and expressing it as a rebuild of the
/// whole list would be both slower and — because a rebuild discards per-row
/// state — wrong.
fn channel_row(app: App, room: &'static crate::model::Room) -> Element {
    let active = move || app.room_id.get() == room.id && !app.settings_open.get();
    let label: Element = text(room.name)
        .with_style(|| StyleApplication::new(styles::RoomRowName::sheet()))
        .into();
    // The unread pip goes quiet the moment you're in the room — a dot on the
    // row you're reading is just noise.
    let pip: Element = runtime_core::view(Vec::new())
        .with_style(move || {
            StyleApplication::new(styles::UnreadDot::sheet())
                .with("shown", if room.unread && !active() { "on" } else { "off" })
        })
        .into();
    press(
        vec![icon(HASH).size(15.0).into(), label, pip],
        move || {
            StyleApplication::new(styles::RoomRow::sheet())
                .with("active", if active() { "on" } else { "off" })
        },
        move || app.go(room.id),
    )
}

/// A live room, with its occupants nested underneath. The faces sit under the
/// room indented past its icon, so the list reads as "who is in there" rather
/// than as a second level of navigation.
fn live_room_row(app: App, room: &'static crate::model::Room) -> Element {
    let active = move || app.room_id.get() == room.id && !app.settings_open.get();
    let occupants = app.occupants(room);
    let count = occupants.len();
    let label: Element = text(room.name)
        .with_style(|| StyleApplication::new(styles::RoomRowName::sheet()))
        .into();
    let mut children = vec![icon(room_icon(room.kind)).size(15.0).into(), label];
    if count > 0 {
        // A fixed third slot on one row, not a reconciled list — nothing here
        // can reorder, so there is no identity for a key to preserve.
        children.push(ui! { Muted(label = count.to_string(), kind = typography_kind::Caption) }); // idealyst-lint-disable-line prefer-keyed-list
    }
    let row = press(
        children,
        move || {
            StyleApplication::new(styles::RoomRow::sheet())
                .with("active", if active() { "on" } else { "off" })
        },
        move || app.go(room.id),
    );

    ui! {
        view() {
            row
            for name in occupants, key = name.clone() {
                room_face(app, room.id, name)
            }
        }
    }
}

/// One occupant under a live room.
fn room_face(app: App, room_id: &'static str, name: String) -> Element {
    let site = format!("face:{room_id}");
    let quiet = app.is_timed_out(&name)
        || name == "Theo Lang"
        || name == "Dad"
        || (name == "You" && !app.mic.get());

    let owned = name.clone();
    person_target(app, &site, &name, move |open| {
        let label: Element = text(owned.clone())
            .with_style(|| StyleApplication::new(styles::RoomRowName::sheet()))
            .into();
        let mut children = vec![ui! { Face(name = owned.clone(), size = AvatarSize::Xs) }, label];
        if quiet {
            children.push(icon(MIC_OFF).size(13.0).into());
        }
        press(children, || StyleApplication::new(styles::RoomFace::sheet()), open)
    })
}

/// Direct messages: avatar, name, the last line, and when it arrived.
fn dm_section(app: App) -> Element {
    ui! {
        view(style = || StyleApplication::new(styles::RoomListPad::sheet())) {
            view(style = || StyleApplication::new(styles::Row::sheet())) {
                SectionLabel(label = "Conversations".to_string())
                Spring()
                IconButton(
                    glyph = String::new(),
                    icon = Some(PLUS),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Sm,
                    on_click = cb(move || app.join_open.set(true)),
                )
            }
            for conversation in DMS.iter().filter(|d| !app.is_removed(d.name)),
                key = conversation.id
            {
                dm_row(app, conversation)
            }
        }
    }
}

/// One conversation: avatar, name, the last line, and when it arrived.
fn dm_row(app: App, conversation: &'static crate::model::Dm) -> Element {
    let active = move || app.room_id.get() == conversation.id && !app.settings_open.get();
    let last = app.messages(conversation.id).pop();
    let preview = match &last {
        Some(l) if l.author == "You" => format!("You: {}", l.text),
        Some(l) => l.text.clone(),
        None => "Say hello".to_string(),
    };
    let stamp = last.map(|l| l.time).unwrap_or_default();
    let name: Element = text(conversation.name)
        .with_style(|| StyleApplication::new(styles::RoomRowName::sheet()))
        .into();

    let summary: Element = ui! {
        view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
            view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                name
                Muted(label = stamp, kind = typography_kind::Overline)
            }
            Muted(label = preview, kind = typography_kind::BodySm)
        }
    };

    let mut children = vec![
        ui! {
            Presence(
                name = conversation.name.to_string(),
                size = AvatarSize::Sm,
                status = conversation.status,
                small = false,
            )
        },
        summary,
    ];
    children.push(
        runtime_core::view(Vec::new())
            .with_style(move || {
                StyleApplication::new(styles::UnreadDot::sheet()).with(
                    "shown",
                    if conversation.unread && !active() { "on" } else { "off" },
                )
            })
            .into(),
    );

    press(
        children,
        move || {
            StyleApplication::new(styles::DmRow::sheet())
                .with("active", if active() { "on" } else { "off" })
        },
        move || app.go(conversation.id),
    )
}

// =============================================================================
// The tray
// =============================================================================

/// The bottom of the room list: the room you're actually in (when you're in
/// one), then you, your mic, and your headphones.
fn tray(app: App) -> Element {
    let in_voice = app.in_voice.get();
    let deafened = app.deafened.get();
    let mic_on = app.mic.get();

    // The strip only exists while you're in a room. It stays put when you
    // navigate away, which is the point: you can read #recipes while still
    // sitting on The Porch, and this is what tells you so.
    let strip: Vec<Element> = if in_voice.is_empty() {
        Vec::new()
    } else {
        let room_name = model::room(in_voice).map(|r| r.name).unwrap_or("");
        let name_node: Element = text(room_name)
            .with_style(|| StyleApplication::new(styles::Ellipsis::sheet()))
            .into();
        vec![ui! {
            view(style = || StyleApplication::new(styles::VoiceStrip::sheet())) {
                // Three bars at staggered heights. The design animates them;
                // this is the same read at rest — see README on the missing
                // keyframe primitive.
                view(style = || StyleApplication::new(styles::Row::sheet())) {
                    view(style = || StyleApplication::new(styles::LevelBar::sheet()).with("step", "a"))
                    view(style = || StyleApplication::new(styles::LevelBar::sheet()).with("step", "b"))
                    view(style = || StyleApplication::new(styles::LevelBar::sheet()).with("step", "c"))
                }
                name_node
                Button(
                    label = "Leave".to_string(),
                    tone = tone::Danger,
                    variant = variant::Ghost,
                    size = size::Sm,
                    shape = shape::Md,
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
                Face(name = "You".to_string(), size = AvatarSize::Sm)
                view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
                    Strong(label = "You".to_string(), kind = typography_kind::BodySm)
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

// =============================================================================
// People sidebar
// =============================================================================

/// Who is around, and who isn't.
#[component]
pub fn PeopleSidebar(#[prop(static)] app: App) -> Element {
    switch(move || (app.space_id.get(), app.people_key()), move |_| people_sidebar_body(app))
}

fn people_sidebar_body(app: App) -> Element {
    let here: Vec<&'static Person> = model::PEOPLE
        .iter()
        .filter(|p| p.status != Status::Offline && !app.is_removed(p.name))
        .collect();
    let away: Vec<&'static Person> = model::PEOPLE
        .iter()
        .filter(|p| p.status == Status::Offline && !app.is_removed(p.name))
        .collect();

    let here_label = format!("Around — {}", here.len());
    let away_label = format!("Away — {}", away.len());

    ui! {
        view(style = || StyleApplication::new(styles::People::sheet())) {
            scroll_view(style = || StyleApplication::new(styles::RoomListBody::sheet())) {
                view(style = || StyleApplication::new(styles::PeoplePad::sheet())) {
                    SectionLabel(label = here_label)
                    for p in here, key = p.name {
                        person_row(app, p)
                    }
                    SectionLabel(label = away_label)
                    for p in away, key = p.name {
                        person_row(app, p)
                    }
                }
            }
        }
    }
}

fn person_row(app: App, p: &'static Person) -> Element {
    let present = if p.status == Status::Offline { "gone" } else { "here" };
    // What we say under a name, in priority order: your own mute wins over a
    // moderator's, which wins over whatever they're up to.
    let note = if app.is_muted(p.name) {
        "Muted for you".to_string()
    } else if app.is_timed_out(p.name) {
        "Muted in rooms".to_string()
    } else {
        p.note.to_string()
    };

    person_target(app, "roster", p.name, move |open| {
        // `Strong`, not the row-name style: `RoomRowName` carries
        // `flex-basis: 0` for horizontal truncation, and inside this column
        // that resolves against the cross axis and collapses the name to zero
        // height.
        let name: Element = ui! { Strong(label = p.name.to_string(), kind = typography_kind::BodySm) };
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
                        name = p.name.to_string(),
                        size = AvatarSize::Xs,
                        status = p.status,
                        small = true,
                    )
                },
                block,
            ],
            move || StyleApplication::new(styles::PersonRow::sheet()).with("present", present),
            open,
        )
    })
}

/// Whether the people sidebar should be showing at all.
pub fn people_visible(app: App) -> bool {
    app.scope.get() == Scope::Space && app.show_people.get()
}
