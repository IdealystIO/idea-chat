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

use crate::model::{App, JoinTab, Status};
use crate::styles;
use crate::net;
use crate::views::bits::{cb, cb1, press, Face, Glyph, Muted, Presence, Strong};

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
fn result_row(app: App, line: crate::model::Line) -> Element {
    let summary: Element = ui! {
        view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
            view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                Strong(label = line.author.clone(), kind = typography_kind::BodySm)
                Muted(label = line.time.clone(), kind = typography_kind::Overline)
            }
            Muted(label = line.text.clone(), kind = typography_kind::BodySm)
        }
    };
    press(
        vec![
            ui! {
                Face(
                    name = line.author.clone(),
                    id = line.author_id.clone().unwrap_or_default(),
                    size = AvatarSize::Sm,
                )
            },
            summary,
        ],
        || StyleApplication::new(styles::ResultRow::sheet()),
        move || app.search_open.set(false),
    )
}

#[component]
fn SearchResults(#[prop(static)] app: App) -> Element {
    switch(move || (app.query.get(), app.space_id.get()), move |_| search_results_body(app))
}

fn search_results_body(app: App) -> Element {
    let hits = app.search_hits();
    let query = app.query.get();
    let nothing_found = hits.is_empty();

    // An empty result set says which kind of empty it is: nothing typed yet,
    // or nothing found. They call for different next moves.
    let empty: Vec<Element> = if nothing_found {
        let label = if query.trim().is_empty() {
            "Search what is loaded in this channel.".to_string()
        } else {
            format!("Nothing matches “{}” in this channel.", query.trim())
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
                for hit in hits, key = hit.id.clone() {
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
    switch(
        move || (app.join_open.get(), app.join_tab.get(), app.minted_invite.get(), app.notice.get()),
        move |_| join_shell(app),
    )
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
    // When a moderator opened this from "Invite someone", the code they can
    // hand out appears here rather than in a second dialog.
    let code = app.minted_invite.get();
    let minted: Vec<Element> = if code.is_empty() {
        Vec::new()
    } else {
        // A whole URL, not a bare code. "Paste this into the second field of
        // the sign-in screen" is an instruction somebody has to follow; a link
        // is not. The app reads `/j/<code>` on load and fills the form itself
        // — see `crate::link`.
        let url = crate::link::invite_url(&crate::link::origin(), &code);
        let copied = app.invite_copied;
        let shown = url.clone();
        vec![ui! {
            Stack(gap = StackGap::Xs) {
                Muted(
                    label = "Share this link. It works once, and expires in a week.".to_string(),
                    kind = typography_kind::BodySm,
                )
                view(style = || StyleApplication::new(styles::CodeBox::sheet())) {
                    Strong(label = shown.clone())
                }
                Button(
                    label = if copied.get() { "Copied".to_string() } else { "Copy link".to_string() },
                    tone = ToneRef::from(tone::Neutral),
                    variant = VariantRef::from(variant::Ghost),
                    shape = shape::Md,
                    block = true,
                    on_click = cb(move || {
                        copied.set(crate::link::copy(&url));
                    }),
                )
            }
        }]
    };

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

            minted

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
                    on_click = cb(move || {
                        if joining {
                            net::join_space(app)
                        } else {
                            net::create_space(app)
                        }
                    }),
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
    switch(move || (app.profile.get(), app.revision.get(), app.muted.get()), move |_| profile_shell(app))
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
    let user_id = app.profile.get();
    if user_id.is_empty() {
        return ui! { view() };
    }
    let Some(member) = app.member(&user_id) else {
        // The roster is the only source of names and it covers the current
        // space, so somebody from a space you have left is a legitimate gap.
        // Saying so beats rendering a card full of blanks.
        return ui! {
            Stack(gap = StackGap::Md) {
                Typography(content = "Not in this space".to_string(), kind = typography_kind::H3)
                Muted(label = "This account is not a member of the space you are looking at."
                    .to_string())
                Stack(axis = StackAxis::Row, gap = StackGap::Sm, justify = StackJustify::End) {
                    Button(
                        label = "Close".to_string(),
                        tone = tone::Neutral,
                        variant = variant::Ghost,
                        shape = shape::Md,
                        on_click = cb(move || app.profile.set(String::new())),
                    )
                }
            }
        };
    };

    let muted_for_me = app.is_muted(&user_id);
    let role_label = {
        let mut r = member.role.clone();
        if let Some(c) = r.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        r
    };
    let is_privileged = member.role != "member";

    let mute_tag: Vec<Element> = if muted_for_me {
        vec![ui! {
            Badge(label = "Muted for you".to_string(), tone = tone::Neutral, variant = variant::Soft)
        }]
    } else {
        Vec::new()
    };

    let message_button: Vec<Element> = if user_id == app.my_id() {
        Vec::new()
    } else {
        let target = user_id.clone();
        vec![ui! {
            Button(
                label = "Send a message".to_string(),
                tone = tone::Primary,
                variant = variant::Filled,
                shape = shape::Md,
                on_click = cb(move || net::open_dm(app, &target)),
            )
        }]
    };

    ui! {
        Stack(gap = StackGap::Md) {
            Stack(axis = StackAxis::Row, gap = StackGap::Md, align = StackAlign::Start) {
                Presence(
                    name = member.display_name.clone(),
                    id = member.id.clone(),
                    size = AvatarSize::Lg,
                    status = Status::Online,
                    small = false,
                )
                Stack(gap = StackGap::Xs) {
                    Typography(content = member.display_name.clone(), kind = typography_kind::H3)
                    Muted(label = format!("@{}", member.handle), kind = typography_kind::BodySm)
                    Stack(axis = StackAxis::Row, gap = StackGap::Xs, wrap = true) {
                        Badge(
                            label = role_label,
                            tone = if is_privileged {
                                ToneRef::from(tone::Primary)
                            } else {
                                ToneRef::from(tone::Neutral)
                            },
                            variant = variant::Soft,
                        )
                        mute_tag
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
