//! The message column: the transcript, and the composer under it.

use icons_lucide::{BOOKMARK, MORE_HORIZONTAL, PAPERCLIP, REPLY, SEND};
use idea_ui::{tone, typography_kind, variant, AvatarSize, Badge, IconButton, IconButtonSize, ToneRef, Typography, VariantRef};
use runtime_core::{rx, switch, component, text, text_input, ui, Element, KeyOutcome, StyleApplication};

use crate::model::{self, App, Line};
use crate::styles;
use crate::views::bits::{cb, press, Face, Muted, Spring};
use crate::views::person_menu::person_target;

#[component]
pub fn ChatColumn(#[prop(static)] app: App) -> Element {
    switch(move || (app.nav_key(), app.content_key(), app.people_key(), app.compact.get()), move |_| chat_body(app))
}

fn chat_body(app: App) -> Element {
    ui! {
        view(style = || StyleApplication::new(styles::Chat::sheet())) {
            transcript(app)
            Composer(app = app)
        }
    }
}

/// The scrolling transcript: an intro block, then the messages.
fn transcript(app: App) -> Element {
    let room_id = app.room_id.get();
    let lines = app.messages(room_id);
    let is_dm = app.viewing_dm();
    let compact = app.compact.get();

    let (intro_title, intro_body) = if is_dm {
        let name = model::dm(room_id).map(|d| d.name).unwrap_or("");
        (
            name.to_string(),
            "Direct messages stay between the two of you. No previews, no receipts you did not ask for."
                .to_string(),
        )
    } else {
        let name = model::room(room_id).map(|r| r.name).unwrap_or("");
        (
            format!("Welcome to #{name}"),
            "The beginning of this channel. Scroll up all you like — this is the top.".to_string(),
        )
    };

    // A message "heads" its group when it follows someone else. Consecutive
    // messages from one person lose the avatar and the name, which is what
    // turns a list of records back into someone talking.
    // Only the *data* is prepared here; the elements are built inside the
    // macro under `key = line.key`, so a message arriving mid-transcript
    // reconciles by identity instead of by position.
    let rows: Vec<Row> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| Row {
            line: line.clone(),
            heads: i == 0 || lines[i - 1].author != line.author,
        })
        .collect();

    ui! {
        scroll_view(style = || StyleApplication::new(styles::Transcript::sheet())) {
            view(style = || StyleApplication::new(styles::Intro::sheet())) {
                Typography(content = intro_title, kind = typography_kind::H2)
                Muted(label = intro_body)
            }

            for row in rows, key = row.line.key.clone() {
                message_row(app, row.line, row.heads, compact)
            }

            // The design pins this to #general. It stands in for a real
            // typing signal, and it renders where the real one would.
            if room_id == "general" {
                view(style = || StyleApplication::new(styles::Intro::sheet())) {
                    Muted(label = "Priya is typing…".to_string(), kind = typography_kind::BodySm)
                }
            }
        }
    }
}

/// A message plus whether it heads its group — the one presentational fact the
/// transcript computes from a message's neighbours rather than from itself.
#[derive(Clone)]
pub struct Row {
    pub line: Line,
    pub heads: bool,
}

/// One message.
fn message_row(app: App, line: Line, heads: bool, compact: bool) -> Element {
    let density = if compact {
        "tight"
    } else if heads {
        "head"
    } else {
        "sub"
    };
    let replies = app.reply_count(&line.key, line.seeded_replies);
    let author = line.author.clone();
    let hidden = app.is_muted(&author);

    // The avatar and the author name are both person targets, so both are
    // built here and splatted — a `person_target` is a wrapper view, not a tag.
    let gutter: Vec<Element> = if heads {
        let site = format!("msg:{}", line.key);
        let name = author.clone();
        vec![person_target(app, &site, &author, move |open| {
            press(
                vec![ui! { Face(name = name.clone(), size = AvatarSize::Sm) }],
                || StyleApplication::new(styles::Bare::sheet()),
                open,
            )
        })]
    } else {
        Vec::new()
    };

    let head: Vec<Element> = if heads {
        let site = format!("name:{}", line.key);
        let name = author.clone();
        let label = author.clone();
        let stamp = line.time.clone();
        vec![ui! {
            view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                {
                    person_target(app, &site, &name, move |open| {
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
        let key = line.key.clone();
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
            move || app.thread_key.set(key.clone()),
        )]
    } else {
        Vec::new()
    };

    let body = line.text.clone();
    let reply_key = line.key.clone();
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
                    on_click = cb(move || app.thread_key.set(reply_key.clone())),
                )
                IconButton(
                    glyph = String::new(),
                    icon = Some(BOOKMARK),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Sm,
                    on_click = cb(|| {}),
                )
                IconButton(
                    glyph = String::new(),
                    icon = Some(MORE_HORIZONTAL),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Sm,
                    on_click = cb(|| {}),
                )
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
    let hint = if app.viewing_dm() {
        let name = model::dm(app.room_id.get()).map(|d| d.name).unwrap_or("");
        format!("Message {}", name.split(' ').next().unwrap_or(name))
    } else {
        format!("Message #{}", model::room(app.room_id.get()).map(|r| r.name).unwrap_or(""))
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
                                app.send();
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
                    on_click = cb(move || app.send()),
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
