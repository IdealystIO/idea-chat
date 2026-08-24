//! The thread panel — a side conversation, docked over the right of the main
//! column so the channel it came from stays visible behind it.

use icons_lucide::{SEND, X};
use idea_ui::{tone, typography_kind, variant, AvatarSize, IconButton, IconButtonSize, ToneRef, VariantRef};
use runtime_core::{rx, switch, component, 
    text, text_input, ui, Element, KeyOutcome, StyleApplication,
};

use crate::model::App;
use crate::net;
use crate::styles;
use crate::views::bits::{cb, Face, Muted, Strong};

#[component]
pub fn ThreadPanel(#[prop(static)] app: App) -> Element {
    switch(move || (app.revision.get(), app.thread_root.get()), move |_| thread_body(app))
}

fn thread_body(app: App) -> Element {
    let lines = app.thread_lines();
    let where_ = app
        .channel()
        .map(|c| if c.kind == "text" { format!("#{}", c.name) } else { c.name })
        .unwrap_or_default();

    ui! {
        view(style = || StyleApplication::new(styles::ThreadPanel::sheet())) {
            view(style = || StyleApplication::new(styles::PanelHead::sheet())) {
                view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
                    Strong(label = "Thread".to_string())
                    Muted(label = where_, kind = typography_kind::Overline)
                }
                IconButton(
                    glyph = String::new(),
                    icon = Some(X),
                    tone = tone::Neutral,
                    variant = variant::Ghost,
                    size = IconButtonSize::Md,
                    on_click = cb(move || app.thread_root.set(String::new())),
                )
            }

            scroll_view(style = || StyleApplication::new(styles::Transcript::sheet())) {
                view(style = || StyleApplication::new(styles::PeoplePad::sheet())) {
                    for line in lines, key = line.id.clone() {
                        reply_row(line)
                    }
                }
            }

            ThreadComposer(app = app)
        }
    }
}

/// The reply box. No `switch`, for the same reason as the channel composer:
/// rebuilding on a keystroke would destroy the focused field.
#[component]
fn ThreadComposer(#[prop(static)] app: App) -> Element {
    ui! {
            view(style = || StyleApplication::new(styles::ComposerWrap::sheet())) {
                view(style = || StyleApplication::new(styles::Composer::sheet())) {
                    {
                        text_input(app.thread_draft, move |v| app.thread_draft.set(v))
                            .placeholder("Reply in thread…".to_string())
                            .on_key_down(move |e| {
                                if e.key == "Enter" && !e.shift {
                                    net::send_reply(app);
                                    KeyOutcome::PreventDefault
                                } else {
                                    KeyOutcome::Default
                                }
                            })
                            .with_style(|| StyleApplication::new(styles::ComposerInput::sheet()))
                    }
                    IconButton(
                        glyph = String::new(),
                        icon = Some(SEND),
                        tone = rx!(if app.thread_draft.get().trim().is_empty() {
                            ToneRef::from(tone::Neutral)
                        } else {
                            ToneRef::from(tone::Primary)
                        }),
                        variant = rx!(if app.thread_draft.get().trim().is_empty() {
                            VariantRef::from(variant::Ghost)
                        } else {
                            VariantRef::from(variant::Filled)
                        }),
                        size = IconButtonSize::Sm,
                        on_click = cb(move || net::send_reply(app)),
                    )
                }
            }
    }
}

/// One message in a thread. Every reply carries its author — a thread is short
/// enough that grouping consecutive messages saves nothing and costs the
/// reader the attribution.
fn reply_row(line: crate::model::Line) -> Element {
    let body: Element = text(line.text.clone())
        .with_style(|| StyleApplication::new(styles::MessageText::sheet()))
        .into();
    ui! {
        view(style = || StyleApplication::new(styles::MessageRow::sheet())
            .with("density", "head"))
        {
            Face(name = line.author.clone(), id = line.author_id.clone().unwrap_or_default(), size = AvatarSize::Sm)
            view(style = || StyleApplication::new(styles::MessageBody::sheet())) {
                view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                    Strong(label = line.author.clone(), kind = typography_kind::BodySm)
                    Muted(label = line.time.clone(), kind = typography_kind::Overline)
                }
                body
            }
        }
    }
}
