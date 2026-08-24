//! The message column: the transcript, and the composer under it.

use icons_lucide::{PAPERCLIP, REPLY, SEND, TRASH_2};
use idea_ui::{tone, typography_kind, variant, AvatarSize, Badge, IconButton, IconButtonSize, ToneRef, Typography, VariantRef};
use runtime_core::{rx, switch, component, text, text_input, ui, Element, KeyOutcome, StyleApplication};

use crate::model::{App, Line};
use crate::styles;
use crate::views::bits::{cb, press, Face, Muted, Spring};
use crate::net;
use crate::views::person_menu::person_target;

#[component]
pub fn ChatColumn(#[prop(static)] app: App) -> Element {
    switch(move || (app.revision.get(), app.channel_id.get(), app.compact.get(), app.muted.get()), move |_| chat_body(app))
}

fn chat_body(app: App) -> Element {
    // No channel, no composer: a message box with nowhere to send is an
    // invitation to type something that will be thrown away.
    let composer: Vec<Element> = if app.channel().is_some() {
        vec![ui! { Composer(app = app) }]
    } else {
        Vec::new()
    };
    ui! {
        view(style = || StyleApplication::new(styles::Chat::sheet())) {
            transcript(app)
            composer
        }
    }
}

/// The scrolling transcript: an intro block, then the messages.
fn transcript(app: App) -> Element {
    // Nothing selected is a real state on a fresh account — there are no
    // channels to select. Rendering the channel intro anyway produces
    // "Welcome to #" and a composer addressed to nobody.
    if app.channel().is_none() {
        return ui! {
            view(style = || StyleApplication::new(styles::Transcript::sheet())) {
                view(style = || StyleApplication::new(styles::Intro::sheet())) {
                    Typography(content = "Nothing open".to_string(), kind = typography_kind::H2)
                    Muted(
                        label = "Make a space or accept an invite, and its channels appear on the left."
                            .to_string(),
                    )
                }
            }
        };
    }
    let is_dm = app.viewing_dm();
    let compact = app.compact.get();
    let name = app.channel().map(|c| c.name).unwrap_or_default();

    let (intro_title, intro_body) = if is_dm {
        (
            name,
            "Direct messages stay between the two of you. No previews, no receipts you did not ask for."
                .to_string(),
        )
    } else {
        (
            format!("Welcome to #{name}"),
            "The beginning of this channel. Scroll up all you like — this is the top.".to_string(),
        )
    };

    let lines = app.lines();
    let empty: Vec<Element> = if lines.is_empty() {
        vec![ui! {
            view(style = || StyleApplication::new(styles::Intro::sheet())) {
                Muted(label = "Nothing here yet. Say something.".to_string())
            }
        }]
    } else {
        Vec::new()
    };
    let rows: Vec<Element> =
        lines.into_iter().map(|line| message_row(app, line, compact)).collect();

    ui! {
        scroll_view(style = || StyleApplication::new(styles::Transcript::sheet())) {
            view(style = || StyleApplication::new(styles::Intro::sheet())) {
                Typography(content = intro_title, kind = typography_kind::H2)
                Muted(label = intro_body)
            }
            empty
            rows
        }
    }
}

/// One message.
fn message_row(app: App, line: Line, compact: bool) -> Element {
    let heads = line.heads;
    let density = if compact {
        "tight"
    } else if heads {
        "head"
    } else {
        "sub"
    };
    let replies = line.reply_count;
    let author_id = line.author_id.clone().unwrap_or_default();
    // A muted person's messages stay readable but recede — hiding them leaves
    // holes in a conversation other people are still replying to.
    let hidden = app.is_muted(&author_id) || line.deleted;

    // The avatar and the author name are both person targets, so both are
    // built here and splatted — a `person_target` is a wrapper view, not a tag.
    let gutter: Vec<Element> = if heads {
        let site = format!("msg:{}", line.id);
        let name = line.author.clone();
        let id = author_id.clone();
        vec![person_target(app, &site, &author_id, move |open| {
            press(
                vec![ui! { Face(name = name.clone(), id = id.clone(), size = AvatarSize::Sm) }],
                || StyleApplication::new(styles::Bare::sheet()),
                open,
            )
        })]
    } else {
        Vec::new()
    };

    let head: Vec<Element> = if heads {
        let site = format!("name:{}", line.id);
        let label = line.author.clone();
        let stamp = if line.edited {
            format!("{} · edited", line.time)
        } else {
            line.time.clone()
        };
        vec![ui! {
            view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                {
                    person_target(app, &site, &author_id, move |open| {
                        press(
                            vec![ui! { text(label.clone()) }],
                            || StyleApplication::new(styles::AuthorName::sheet()),
                            open,
                        )
                    })
                }
                Muted(label = stamp, kind = typography_kind::Overline)
            }
        }]
    } else {
        Vec::new()
    };

    let thread_badge: Vec<Element> = if replies > 0 {
        let key = line.id.clone();
        vec![press(
            vec![ui! {
                Badge(
                    label = if replies == 1 {
                        "1 reply".to_string()
                    } else {
                        format!("{replies} replies")
                    },
                    tone = tone::Primary,
                    variant = variant::Soft,
                )
            }],
            || StyleApplication::new(styles::Hug::sheet()),
            move || net::open_thread(app, &key),
        )]
    } else {
        Vec::new()
    };

    let body = line.text.clone();
    let reply_key = line.id.clone();
    let delete_id = line.id.clone();
    let mine = line.author_id.as_deref() == Some(app.my_id().as_str());
    let body_node: Element = text(body)
        .with_style(move || {
            StyleApplication::new(styles::MessageText::sheet())
                .with("muted", if hidden { "on" } else { "off" })
        })
        .into();

    // One splat rather than three: inside `ui!`, a bare-identifier splat
    // followed by a braced child is parsed as a tag with a children block.
    let mut body_block = head;
    body_block.push(body_node);
    body_block.extend(thread_badge);

    // Only offered where it would work: your own message always, anyone's if
    // you moderate this space. A button that always 403s is worse than none.
    let delete_button: Vec<Element> = if mine || app.moderates() {
        vec![ui! {
            IconButton(
                glyph = String::new(),
                icon = Some(TRASH_2),
                tone = tone::Danger,
                variant = variant::Ghost,
                size = IconButtonSize::Sm,
                on_click = cb(move || net::delete_message(app, &delete_id)),
            )
        }]
    } else {
        Vec::new()
    };

    ui! {
        view(style = move || StyleApplication::new(styles::MessageRow::sheet())
            .with("density", density))
        {
            view(style = || StyleApplication::new(styles::Gutter::sheet())) {
                gutter
            }

            view(style = || StyleApplication::new(styles::MessageBody::sheet())) {
                body_block
            }

            // The hover actions. Always mounted; the stylesheet's `shown` axis
            // is what reveals them, so hovering costs no mount and the row
            // never shifts as they appear.
            view(style = || StyleApplication::new(styles::MessageActions::sheet())) {
                IconButton(
                    glyph = String::new(),
                    icon = Some(REPLY),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Sm,
                    on_click = cb(move || net::open_thread(app, &reply_key)),
                )
                delete_button
            }
        }
    }
}

/// The composer. The box is the chrome; the input inside it is bare.
///
/// Deliberately NOT wrapped in a `switch`. The two things that change while
/// you type — the focus ring and whether Send is lit — are painted through
/// reactive sinks instead: the box's style closure reads `composer_focused`
/// live, and Send's tone/variant are `rx!` expressions. A `switch` here would
/// be actively wrong, not merely wasteful: focusing the box sets
/// `composer_focused`, which would change the key, which would rebuild the
/// subtree and destroy the very `<input>` that just took focus — so the field
/// could never be typed into at all.
#[component]
fn Composer(#[prop(static)] app: App) -> Element {
    let name = app.channel().map(|c| c.name).unwrap_or_default();
    let hint = if app.viewing_dm() {
        format!("Message {}", name.split(' ').next().unwrap_or(&name))
    } else {
        format!("Message #{name}")
    };

    ui! {
        view(style = || StyleApplication::new(styles::ComposerWrap::sheet())) {
            // Reads the signal INSIDE the closure, so the border re-tints in
            // place rather than remounting the field.
            view(style = move || StyleApplication::new(styles::Composer::sheet())
                .with("focused", if app.composer_focused.get() { "on" } else { "off" }))
            {
                {
                    text_input(app.draft, move |v| app.draft.set(v))
                        .placeholder(hint)
                        .on_focus(move |f| app.composer_focused.set(f))
                        // Enter sends, Shift+Enter is a newline. Swallowing the
                        // key matters: without `PreventDefault` the newline
                        // lands in the box we just cleared.
                        .on_key_down(move |e| {
                            if e.key == "Enter" && !e.shift {
                                net::send(app);
                                KeyOutcome::PreventDefault
                            } else {
                                KeyOutcome::Default
                            }
                        })
                        .with_style(|| StyleApplication::new(styles::ComposerInput::sheet()))
                }
                IconButton(
                    glyph = String::new(),
                    icon = Some(PAPERCLIP),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Md,
                    on_click = cb(|| {}),
                )
                IconButton(
                    glyph = String::new(),
                    icon = Some(SEND),
                    tone = rx!(if app.draft.get().trim().is_empty() {
                        ToneRef::from(tone::Neutral)
                    } else {
                        ToneRef::from(tone::Primary)
                    }),
                    variant = rx!(if app.draft.get().trim().is_empty() {
                        VariantRef::from(variant::Ghost)
                    } else {
                        VariantRef::from(variant::Filled)
                    }),
                    size = IconButtonSize::Md,
                    on_click = cb(move || net::send(app)),
                )
            }
            view(style = || StyleApplication::new(styles::ComposerHint::sheet())) {
                text("Enter sends · Shift+Enter for a new line")
                Spring()
                text("No bots. No slash commands. Just people.")
            }
        }
    }
}
