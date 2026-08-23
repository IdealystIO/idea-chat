//! The four dialogs: search, join/create a space, a person's card, and the
//! remove confirmation.
//!
//! All four are `idea_ui::Modal`. It owns the scrim, the Escape/back route,
//! the backdrop tap, and the enter/exit animation — none of which is worth
//! re-deriving per dialog, and all of which a hand-rolled overlay gets subtly
//! wrong on one backend or another.
//!
//! One departure from the design: the search palette sits centred rather than
//! near the top of the window. `Modal` centres its surface, and a bespoke
//! top-anchored overlay would have meant giving up its Escape handling and its
//! backdrop semantics to move a dialog 200px up the screen.

use icons_lucide::SEARCH;
use idea_ui::{shape, tone, typography_kind, variant, AvatarSize, Badge, Button, Divider, Field, Modal, Stack, StackAlign, StackAxis, StackGap, StackJustify, StackPadding, ToneRef, Typography, VariantRef};
use runtime_core::primitives::text_input::TextInputHandle;
use runtime_core::{
    after_ms, component, switch, text, text_input, ui, Element, Ref,
    StyleApplication,
};

use crate::model::{self, App, JoinTab, Status};
use crate::styles;
use crate::views::bits::{cb, cb1, press, Face, Glyph, Muted, Presence, SectionLabel, Strong};

// =============================================================================
// Search
// =============================================================================

#[component]
pub fn SearchPalette(#[prop(static)] app: App) -> Element {
    switch(move || (app.search_open.get(), app.space_id.get()), move |_| search_shell(app))
}

fn search_shell(app: App) -> Element {
    ui! {
        Modal(
            open = app.search_open,
            width = 620.0,
            on_dismiss = Some(cb(move || app.search_open.set(false))),
            content = move || search_body(app),
        )
    }
}

fn search_body(app: App) -> Element {
    // Focus on mount. A command palette that needs a click before it accepts
    // typing is a command palette nobody uses twice.
    //
    // Deferred by a tick: a `Ref` is filled when its primitive mounts, which
    // has not happened yet at the point this function returns its tree.
    let focus: Ref<TextInputHandle> = Ref::new();
    let field: Element = text_input(app.query, move |v| app.query.set(v))
        .placeholder("Search this space".to_string())
        .bind(focus.clone())
        .with_style(|| StyleApplication::new(styles::BareInput::sheet()))
        .into();
    // NOTE: this currently does nothing on web. The `Ref` fills and `focus()`
    // runs, but `Modal` moves focus to its own card layer afterwards and there
    // is no prop to opt out — so the palette opens unfocused however long the
    // delay. Kept because it is the correct call and costs nothing; see the
    // framework findings in README.md.
    after_ms(250, move || {
        if let Some(handle) = focus.get() {
            handle.focus();
        }
    });
    let hits = ui! { SearchResults(app = app) };
    let cap: Element = text("esc")
        .with_style(|| StyleApplication::new(styles::KeyCap::sheet()))
        .into();
    let bar: Vec<Element> = vec![ui! { Glyph(data = Some(SEARCH), size = 17.0) }, field, cap];

    ui! {
        view(style = || StyleApplication::new(styles::Column::sheet())) {
            view(style = || StyleApplication::new(styles::SearchBar::sheet())) {
                bar
            }
            hits
        }
    }
}

/// One search result.
fn result_row(app: App, hit: crate::model::Hit) -> Element {
    let room = hit.room;
    let summary: Element = ui! {
        view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
            view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                Strong(label = hit.author.clone(), kind = typography_kind::BodySm)
                Muted(label = hit.where_.clone(), kind = typography_kind::Overline)
            }
            Muted(label = hit.text.clone(), kind = typography_kind::BodySm)
        }
    };
    press(
        vec![ui! { Face(name = hit.author.clone(), size = AvatarSize::Sm) }, summary],
        || StyleApplication::new(styles::ResultRow::sheet()),
        move || app.go(room),
    )
}

#[component]
fn SearchResults(#[prop(static)] app: App) -> Element {
    switch(move || (app.query.get(), app.space_id.get()), move |_| search_results_body(app))
}

fn search_results_body(app: App) -> Element {
    let hits = app.search();
    let query = app.query.get();
    let nothing_found = hits.is_empty();

    // An empty result set says which kind of empty it is: nothing typed yet,
    // or nothing found. They call for different next moves.
    let empty: Vec<Element> = if nothing_found {
        let label = if query.trim().is_empty() {
            "Type to search messages and people.".to_string()
        } else {
            format!("Nothing matches “{}”.", query.trim())
        };
        vec![ui! {
            Stack(padding = StackPadding::Xl, align = StackAlign::Center) {
                Muted(label = label)
            }
        }]
    } else {
        Vec::new()
    };

    ui! {
        scroll_view(style = || StyleApplication::new(styles::ResultsScroll::sheet())) {
            Stack(gap = StackGap::None) {
                for hit in hits, key = hit.key.clone() {
                    result_row(app, hit)
                }
                empty
            }
        }
    }
}

// =============================================================================
// Join / create a space
// =============================================================================

#[component]
pub fn JoinDialog(#[prop(static)] app: App) -> Element {
    switch(move || (app.join_open.get(), app.join_tab.get()), move |_| join_shell(app))
}

fn join_shell(app: App) -> Element {
    ui! {
        Modal(
            open = app.join_open,
            width = 540.0,
            on_dismiss = Some(cb(move || app.join_open.set(false))),
            content = move || join_body(app),
        )
    }
}

fn join_body(app: App) -> Element {
    let joining = app.join_tab.get() == JoinTab::Join;

    ui! {
        Stack(gap = StackGap::Lg) {
            view(style = || StyleApplication::new(styles::Column::sheet())) {
                Typography(content = "Somewhere to be".to_string(), kind = typography_kind::H2)
                Muted(label = "Spaces are small by default. You can always make another."
                    .to_string())
            }

            Stack(axis = StackAxis::Row, gap = StackGap::Sm) {
                Button(
                    label = "Join with a link".to_string(),
                    tone = if joining { ToneRef::from(tone::Primary) } else { ToneRef::from(tone::Neutral) },
                    variant = if joining {
                        VariantRef::from(variant::Filled)
                    } else {
                        VariantRef::from(variant::Ghost)
                    },
                    shape = shape::Md,
                    block = true,
                    on_click = cb(move || {
                        app.join_tab.set(JoinTab::Join);
                        app.join_value.set(String::new());
                    }),
                )
                Button(
                    label = "Start a new one".to_string(),
                    tone = if joining { ToneRef::from(tone::Neutral) } else { ToneRef::from(tone::Primary) },
                    variant = if joining {
                        VariantRef::from(variant::Ghost)
                    } else {
                        VariantRef::from(variant::Filled)
                    },
                    shape = shape::Md,
                    block = true,
                    on_click = cb(move || {
                        app.join_tab.set(JoinTab::New);
                        app.join_value.set(String::new());
                    }),
                )
            }

            Field(
                label = Some(if joining {
                    "Invite link".to_string()
                } else {
                    "Name your space".to_string()
                }),
                value = app.join_value,
                on_change = cb1(move |v: String| app.join_value.set(v)),
                placeholder = Some(if joining {
                    "hearth.chat/j/quiet-porch".to_string()
                } else {
                    "The Cabin".to_string()
                }),
                help = Some(if joining {
                    "Links last a week and only work once. Ask whoever invited you for a fresh one if it has expired.".to_string()
                } else {
                    "You get four channels and two rooms to start. Rename or delete any of them later.".to_string()
                }),
            )

            Stack(axis = StackAxis::Row, gap = StackGap::Sm, justify = StackJustify::End) {
                Button(
                    label = "Not now".to_string(),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    shape = shape::Md,
                    on_click = cb(move || app.join_open.set(false)),
                )
                Button(
                    label = if joining { "Join space".to_string() } else { "Create space".to_string() },
                    tone = tone::Primary,
                    variant = variant::Filled,
                    shape = shape::Md,
                    on_click = cb(move || app.join_open.set(false)),
                )
            }
        }
    }
}

// =============================================================================
// A person
// =============================================================================

#[component]
pub fn ProfileCard(#[prop(static)] app: App) -> Element {
    switch(move || (app.profile.get(), app.people_key(), app.space_id.get()), move |_| profile_shell(app))
}

fn profile_shell(app: App) -> Element {
    let open = !app.profile.get().is_empty();
    ui! {
        Modal(
            open = open,
            width = 352.0,
            on_dismiss = Some(cb(move || app.profile.set(String::new()))),
            content = move || profile_body(app),
        )
    }
}

fn profile_body(app: App) -> Element {
    let name = app.profile.get();
    if name.is_empty() {
        return ui! { view() };
    }
    let status = model::status_of(&name);
    let bio = model::bio(&name);
    let is_mod = app.is_moderator(&name);
    let muted_for_me = app.is_muted(&name);
    let has_dm = model::dm_for(&name).is_some();

    // Which rooms this person is usually found in. An empty list reads as a
    // sentence rather than a gap — a blank area under a heading looks broken.
    let rooms: Vec<&'static str> = model::rooms_in(app.space_id.get())
        .filter(|r| r.people.contains(&name.as_str()))
        .map(|r| r.name)
        .collect();
    let no_rooms: Vec<Element> = if rooms.is_empty() {
        vec![ui! {
            Muted(label = "Not in a room right now".to_string(), kind = typography_kind::BodySm)
        }]
    } else {
        Vec::new()
    };

    let mute_tag: Vec<Element> = if muted_for_me {
        vec![ui! {
            Badge(label = "Muted for you".to_string(), tone = tone::Neutral, variant = variant::Soft)
        }]
    } else {
        Vec::new()
    };

    let message_button: Vec<Element> = if has_dm {
        let target = name.clone();
        vec![ui! {
            Button(
                label = "Send a message".to_string(),
                tone = tone::Primary,
                variant = variant::Filled,
                shape = shape::Md,
                on_click = cb(move || {
                    let id = model::dm_for(&target).map(|d| d.id);
                    app.profile.set(String::new());
                    if let Some(id) = id {
                        app.go(id);
                    }
                }),
            )
        }]
    } else {
        Vec::new()
    };

    let presence_label = match status {
        Status::Online => "Around now",
        Status::Away => "Away",
        Status::Offline => "Offline",
    };

    ui! {
        Stack(gap = StackGap::Md) {
            Stack(axis = StackAxis::Row, gap = StackGap::Md, align = StackAlign::Start) {
                Presence(name = name.clone(), size = AvatarSize::Lg, status = status, small = false)
                Stack(gap = StackGap::Xs) {
                    Typography(content = name.clone(), kind = typography_kind::H3)
                    Muted(label = presence_label.to_string(), kind = typography_kind::BodySm)
                    Stack(axis = StackAxis::Row, gap = StackGap::Xs, wrap = true) {
                        Badge(
                            label = if is_mod { "Moderator".to_string() } else { "Member".to_string() },
                            tone = if is_mod { ToneRef::from(tone::Primary) } else { ToneRef::from(tone::Neutral) },
                            variant = variant::Soft,
                        )
                        mute_tag
                    }
                }
            }

            Stack(gap = StackGap::Xs) {
                Muted(
                    label = bio.map(|b| b.where_).unwrap_or("In this space").to_string(),
                    kind = typography_kind::BodySm,
                )
                Muted(
                    label = format!(
                        "In this space since {}",
                        bio.map(|b| b.since).unwrap_or("a while back")
                    ),
                    kind = typography_kind::BodySm,
                )
            }

            Divider()

            Stack(gap = StackGap::Sm) {
                SectionLabel(label = "Rooms they're usually in".to_string())
                view(style = || StyleApplication::new(styles::WrapRow::sheet())) {
                    no_rooms
                    for room in rooms, key = room {
                        Badge(label = room.to_string(), tone = tone::Neutral, variant = variant::Soft)
                    }
                }
            }

            Divider()

            Stack(axis = StackAxis::Row, gap = StackGap::Sm, justify = StackJustify::End) {
                message_button
                Button(
                    label = "Close".to_string(),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    shape = shape::Md,
                    on_click = cb(move || app.profile.set(String::new())),
                )
            }
        }
    }
}

// =============================================================================
// Remove confirmation
// =============================================================================

#[component]
pub fn RemoveConfirm(#[prop(static)] app: App) -> Element {
    switch(move || app.remove_target.get(), move |_| remove_shell(app))
}

fn remove_shell(app: App) -> Element {
    let open = !app.remove_target.get().is_empty();
    ui! {
        Modal(
            open = open,
            width = 400.0,
            on_dismiss = Some(cb(move || app.remove_target.set(String::new()))),
            content = move || remove_body(app),
        )
    }
}

fn remove_body(app: App) -> Element {
    let name = app.remove_target.get();
    let first = name.split(' ').next().unwrap_or(&name).to_string();

    ui! {
        Stack(gap = StackGap::Md) {
            Typography(content = format!("Remove {name}?"), kind = typography_kind::H3)
            Muted(label = format!(
                "Their messages stay where they are. {first} would need a new invite to come back."
            ))
            Stack(axis = StackAxis::Row, gap = StackGap::Sm, justify = StackJustify::End) {
                Button(
                    label = "Keep them".to_string(),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    shape = shape::Md,
                    on_click = cb(move || app.remove_target.set(String::new())),
                )
                Button(
                    label = "Remove".to_string(),
                    tone = tone::Danger,
                    variant = variant::Filled,
                    shape = shape::Md,
                    on_click = cb(move || app.confirm_remove()),
                )
            }
        }
    }
}
