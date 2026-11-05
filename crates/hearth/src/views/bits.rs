//! Small shared pieces used across more than one view.
//!
//! The rule for what earns a place here: it appears in three or more views and
//! composing it inline each time would mean repeating the *same* arrangement
//! of idea-ui components, not merely the same component. An avatar is
//! `idea_ui::Avatar` — it does not belong here. An avatar with a presence dot
//! anchored to its corner does, because that pairing is Hearth's, and it shows
//! up in the roster, the DM list, the room faces, and the profile card.
//!
//! These are `#[component]`s rather than `fn(..) -> Element` helpers for the
//! reason the framework's own standards give: anything that takes parameters
//! and is called from more than one place should be a tag, so call sites read
//! `Muted(label = …)` instead of threading positional arguments. It also keeps
//! every child inside `ui!` a tag, which is what the macro parses cleanly — a
//! braced `{ expr }` sibling immediately after a childless component tag is
//! taken as that component's children block.

use idea_ui::{tone, typography_kind, Avatar, AvatarSize, ToneRef, Typography, TypographyKindRef};
use runtime_core::{component, icon, pressable, text, ui, Element, IconData, StyleApplication};
use std::rc::Rc;

use crate::model::{initials, tone_index, Status};
use crate::styles;

/// The six avatar tints, indexed by [`tone_index`].
///
/// Keyed on a person's **id**, so renaming yourself does not recolour every
/// message you have ever sent.
pub fn avatar_color(id: &str) -> ToneRef {
    match tone_index(id) {
        0 => tone::Primary.into(),
        1 => tone::Info.into(),
        2 => tone::Warning.into(),
        3 => tone::Success.into(),
        4 => tone::Danger.into(),
        _ => tone::Neutral.into(),
    }
}

/// Someone's avatar: initials from their name, colour from their id.
///
/// Both are needed because they answer different questions — the letters say
/// who this is, the colour keeps saying it after a rename.
#[component]
pub fn Face(
    // `#[prop(static)]`: read to derive initials and a tint, not routed into a
    // reactive sink, so wrapping it in `Reactive` would only mean unwrapping it
    // again on the next line.
    #[prop(static)] name: String,
    #[prop(static)] id: String,
    #[prop(static, default = AvatarSize::Sm)] size: AvatarSize,
) -> Element {
    ui! {
        Avatar(
            initials = initials(&name),
            tone = avatar_color(&id),
            size = size,
        )
    }
}

/// An avatar with a presence dot on its corner.
///
/// `small` picks the 9px dot for a 24px avatar; the 10px dot is for 32px and
/// up. Below 24px the dot stops being legible and starts being noise, which is
/// why the room-face avatars don't carry one.
#[component]
pub fn Presence(
    #[prop(static)] name: String,
    #[prop(static)] id: String,
    #[prop(static, default = AvatarSize::Sm)] size: AvatarSize,
    #[prop(static, default = Status::Offline)] status: Status,
    #[prop(static)] small: bool,
) -> Element {
    let dot_status = match status {
        Status::Online => "online",
        Status::Away => "away",
        Status::Offline => "offline",
    };
    let dot_size = if small { "sm" } else { "md" };
    ui! {
        view(style = || StyleApplication::new(styles::AvatarSlot::sheet())) {
            Face(name = name, id = id, size = size)
            view(style = move || StyleApplication::new(styles::PresenceDot::sheet())
                .with("status", dot_status)
                .with("size", dot_size))
        }
    }
}

/// A section heading in a sidebar: TALK, ROOMS, AROUND.
#[component]
pub fn SectionLabel(label: String) -> Element {
    ui! {
        text(label).with_style(|| StyleApplication::new(styles::GroupLabel::sheet()))
    }
}

/// Muted small print — the meta line under a space name, a timestamp, a hint.
#[component]
pub fn Muted(
    label: String,
    #[prop(default = TypographyKindRef::from(typography_kind::Body))] kind: TypographyKindRef,
) -> Element {
    ui! {
        Typography(content = label, kind = kind, muted = true)
    }
}

/// Emphasised body text — a name, a row title. `Typography` with a weight
/// override, which is the pairing Hearth reaches for constantly.
#[component]
pub fn Strong(
    label: String,
    #[prop(default = TypographyKindRef::from(typography_kind::Body))] kind: TypographyKindRef,
) -> Element {
    ui! {
        Typography(
            content = label,
            kind = kind,
            weight = Some(runtime_core::FontWeight::SemiBold),
        )
    }
}

/// A bare vector icon at a pinned size. An icon has no intrinsic size, so it
/// collapses to nothing under flex unless one is set.
#[component]
pub fn Glyph(
    #[prop(static, default = 15.0)] size: f32,
    #[prop(static)] data: Option<IconData>,
) -> Element {
    match data {
        Some(d) => icon(d).size(size).into(),
        None => ui! { view() },
    }
}

/// The `flex: 1` spacer that pushes what follows to the far end of a row.
#[component]
pub fn Spring() -> Element {
    ui! { view(style = || StyleApplication::new(styles::Spring::sheet())) }
}

/// Wrap a closure as the `Rc<dyn Fn()>` every idea-ui callback prop wants.
///
/// `Rc::new(closure)` produces `Rc<{closure}>`, which does not coerce through
/// the `.into()` the `ui!` macro applies to prop values — the unsize has to
/// happen at a typed binding. This is that binding, once, instead of a
/// turbofish at every call site.
pub fn cb(f: impl Fn() + 'static) -> Rc<dyn Fn()> {
    Rc::new(f)
}

/// [`cb`] for the one-argument callbacks (`on_change`).
pub fn cb1<T>(f: impl Fn(T) + 'static) -> Rc<dyn Fn(T)> {
    Rc::new(f)
}

/// A layout box made clickable without inheriting any button chrome.
///
/// idea-ui's `Button`/`IconButton` are the right answer wherever a control
/// should *look* like a control. This is for the rows that are the content —
/// a channel in the list, a message author, a search result — where the press
/// target has to be the row itself and the styling is the row's own.
pub fn press(
    children: Vec<Element>,
    style: impl Fn() -> StyleApplication + 'static,
    on_click: impl Fn() + 'static,
) -> Element {
    pressable(children, on_click).with_style(style).into()
}
