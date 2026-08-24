//! The mobile preview — the same app in one column, shown inside a phone
//! frame so the desktop layout and the phone layout can be compared without
//! leaving the desktop.
//!
//! This is a *preview*, not the phone build. On a real phone the shell would
//! be a tab navigator over the same views; what this shows is the shape that
//! collapses into: one column, four tabs, and nothing else. Everything it
//! renders reads from the same `App` state the desktop does, so it can never
//! drift out of sync with what it is previewing.

use icons_lucide::{CHEVRON_LEFT, PHONE, SEND};
use idea_ui::{
    shape, size, tone, typography_kind, variant, AvatarSize, Button, Stack, StackAlign, StackAxis,
    StackGap, Typography,
};
use runtime_core::{component, switch, text, ui, Element, StyleApplication};

use crate::model::App;
use crate::styles;
use crate::views::bits::{cb, Face, Glyph, Muted, Spring, Strong};

/// The four tabs a phone collapses the whole app into.
///
/// A named struct rather than a tuple: `ui!`'s keyed `for` warns "unused
/// variable" on every binding after the first when the item is destructured,
/// because the macro expands the pattern once per item and once for the key.
#[derive(Clone)]
struct Tab {
    label: &'static str,
    active: bool,
}

const TABS: &[Tab] = &[
    Tab { label: "Chats", active: true },
    Tab { label: "Rooms", active: false },
    Tab { label: "People", active: false },
    Tab { label: "You", active: false },
];

/// The preview is the one overlay built on a bare scrim rather than
/// `idea_ui::Modal`.
///
/// Not a stylistic choice: `Modal` caps its surface at ~520dp whatever its
/// `width` prop says (see the framework findings in README.md), and a 372dp
/// phone beside a 280dp note needs 680. `Modal` is still the right component
/// for the four actual dialogs — they're all narrower than the cap, and they
/// want its focus trap and Escape handling, which a presentation surface like
/// this one does not.
#[component]
pub fn MobilePreview(#[prop(static)] app: App) -> Element {
    switch(move || app.mobile_open.get(), move |open| {
        if !*open {
            return ui! { view() };
        }
        let dismiss = press_backdrop(app);
        ui! {
            view(style = || StyleApplication::new(styles::Scrim::sheet())
                .with("depth", "deep"))
            {
                dismiss
                MobileBody(app = app)
            }
        }
    })
}

/// The full-bleed press target behind the phone: clicking outside closes.
fn press_backdrop(app: App) -> Element {
    crate::views::bits::press(
        Vec::new(),
        || StyleApplication::new(styles::ScrimCatcher::sheet()),
        move || app.mobile_open.set(false),
    )
}

#[component]
fn MobileBody(#[prop(static)] app: App) -> Element {
    switch(move || (app.revision.get(), app.channel_id.get()), move |_| mobile_body(app))
}

fn mobile_body(app: App) -> Element {
    let channel = app.channel();
    let title = channel
        .as_ref()
        .map(|c| if c.kind == "text" { format!("# {}", c.name) } else { c.name.clone() })
        .unwrap_or_default();
    let space_name = app.space().map(|s| s.name).unwrap_or_default();

    // The last handful — a phone screen holds about six.
    let all = app.lines();
    let recent: Vec<crate::model::Line> =
        all.iter().skip(all.len().saturating_sub(6)).cloned().collect();

    ui! {
        Stack(axis = StackAxis::Row, gap = StackGap::Xl, align = StackAlign::Center) {
            view(style = || StyleApplication::new(styles::Phone::sheet())) {
                view(style = || StyleApplication::new(styles::PhoneStatus::sheet())) {
                    text("9:41")
                    Muted(label = "▁▄ ⏻".to_string(), kind = typography_kind::Caption)
                }

                view(style = || StyleApplication::new(styles::PhoneBar::sheet())) {
                    Glyph(data = Some(CHEVRON_LEFT), size = 19.0)
                    view(style = || StyleApplication::new(styles::Ellipsis::sheet())) {
                        Strong(label = title, kind = typography_kind::BodyLg)
                        Muted(label = space_name, kind = typography_kind::Overline)
                    }
                    view(style = || StyleApplication::new(styles::PhoneCall::sheet())) {
                        Glyph(data = Some(PHONE), size = 16.0)
                    }
                }

                view(style = || StyleApplication::new(styles::Transcript::sheet())) {
                    for line in recent, key = line.id.clone() {
                        phone_row(line)
                    }
                }

                // The composer is a pill rather than a box: on a phone it
                // shares the bottom edge with the tab bar, and a squared box
                // against a squared bar reads as one grey slab.
                view(style = || StyleApplication::new(styles::PhoneComposerPad::sheet())) {
                    view(style = || StyleApplication::new(styles::PhoneComposer::sheet())) {
                        Muted(label = "Message".to_string())
                        Spring()
                        view(style = || StyleApplication::new(styles::PhoneSend::sheet())) {
                            Glyph(data = Some(SEND), size = 15.0)
                        }
                    }
                }

                view(style = || StyleApplication::new(styles::PhoneTabs::sheet())) {
                    for tab in TABS, key = tab.label {
                        view(style = move || StyleApplication::new(styles::PhoneTab::sheet())
                            .with("active", if tab.active { "on" } else { "off" }))
                        {
                            view(style = move || StyleApplication::new(styles::PhoneTabDot::sheet())
                                .with("active", if tab.active { "on" } else { "off" }))
                            text(tab.label)
                        }
                    }
                }
            }

            aside(app)
        }
    }
}

/// One message on the phone. Same grouping rule as the desktop transcript —
/// the layout narrows, the reading model doesn't change.
fn phone_row(line: crate::model::Line) -> Element {
    let heads = line.heads;
    let avatar: Vec<Element> = if heads {
        vec![ui! { Face(name = line.author.clone(), id = line.author_id.clone().unwrap_or_default(), size = AvatarSize::Sm) }]
    } else {
        Vec::new()
    };
    let head: Vec<Element> = if heads {
        vec![ui! {
            view(style = || StyleApplication::new(styles::MessageHead::sheet())) {
                Strong(label = line.author.clone(), kind = typography_kind::BodySm)
                Muted(label = line.time.clone(), kind = typography_kind::Overline)
            }
        }]
    } else {
        Vec::new()
    };
    let body: Element = text(line.text.clone())
        .with_style(|| StyleApplication::new(styles::MessageText::sheet()))
        .into();
    let mut block = head;
    block.push(body);

    ui! {
        view(style = || StyleApplication::new(styles::MessageRow::sheet())
            .with("density", "sub"))
        {
            view(style = || StyleApplication::new(styles::Gutter::sheet())) {
                avatar
            }
            view(style = || StyleApplication::new(styles::MessageBody::sheet())) {
                block
            }
        }
    }
}

/// The note beside the phone.
fn aside(app: App) -> Element {
    ui! {
        view(style = || StyleApplication::new(styles::PhoneAside::sheet())) {
            Typography(content = "Same app, one column".to_string(), kind = typography_kind::H3)
            Muted(label = "Rooms, chat and calls collapse into four tabs. Nothing new to learn, nothing extra to find."
                .to_string())
            Stack(axis = StackAxis::Row, align = StackAlign::Start) {
                Button(
                    label = "Back to desktop".to_string(),
                    tone = tone::Neutral,
                    variant = variant::Soft,
                    size = size::Md,
                    shape = shape::Md,
                    on_click = cb(move || app.mobile_open.set(false)),
                )
            }
        }
    }
}
