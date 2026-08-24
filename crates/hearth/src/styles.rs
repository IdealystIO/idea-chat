//! Hearth's chrome.
//!
//! Everything idea-ui ships a component for — buttons, badges, avatars,
//! cards, typography, the modal shell — is used as-is. What lives here is the
//! *layout*: the three-column shell, the rails and panels that hold those
//! components, and the handful of surfaces (a message row, a room tile) that
//! are genuinely app furniture rather than reusable widgets.
//!
//! Every value reads a token (`t.color.*`, `t.spacing.*`, `t.radius.*`,
//! `t.intent.*`), so the light/dark swap is free and the accent change reaches
//! the chrome as well as the components. No literal colours: a hex here is a
//! panel that stays light when the app goes dark.

use idea_ui::IdeaThemeRef;
use runtime_core::stylesheet;
use runtime_core::{
    AlignItems, AlignSelf, Color, FlexDirection, FontWeight, JustifyContent, Length, Overflow,
    Position, TextAlign, TextTransform,
};

// =============================================================================
// Shell
// =============================================================================

// The whole window: three columns, no page scroll. Every scrolling
// region inside owns its own overflow, which is what keeps the composer
// and the room list pinned while the transcript moves.
stylesheet! {
    pub Shell<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            width: Length::pct(100.0),
            height: Length::pct(100.0),
            overflow: Overflow::Hidden,
            background: t.color.background(),
            color: t.color.text(),
        }
        transitions {
            background: 220ms EaseInOut,
            color: 220ms EaseInOut,
        }
    }
}

// =============================================================================
// Spaces rail — the 70px column of space buttons
// =============================================================================

// The navigation surface — rail beside room list — as one panel. AppShell
// pins this in-flow on desktop and slides it in as a drawer below Lg, so it
// has to fill whatever it is given rather than set a width of its own.
stylesheet! {
    pub NavPanel<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            width: Length::pct(100.0),
            height: Length::pct(100.0),
            overflow: Overflow::Hidden,
        }
    }
}

stylesheet! {
    pub Rail<IdeaThemeRef> {
        base(t) {
            width: 70.0,
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            gap: 6.0,
            padding_vertical: t.spacing.md(),
            border_right_width: 1.0,
            border_right_color: t.color.border(),
        }
        transitions { border_right_color: 220ms EaseInOut }
    }
}

// A rail slot: the button plus room for the selection pip at its left.
stylesheet! {
    pub RailSlot<()> {
        base(_t) {
            width: Length::pct(100.0),
            position: Position::Relative,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            padding_vertical: 2.0,
            flex_shrink: 0.0,
        }
    }
}

// The selection pip — a stub of ink against the left edge that grows
// when its space is the active one. Discord's tell, and it survives
// because it reads at a glance without colour.
stylesheet! {
    pub RailPip<IdeaThemeRef> {
        base(t) {
            position: Position::Absolute,
            left: 0.0,
            width: 3.0,
            height: 0.0,
            border_top_right_radius: 3.0,
            border_bottom_right_radius: 3.0,
            background: t.color.text(),
        }
        variant state {
            #[default]
            idle(_t) {}
            hovered(_t) { height: 10.0 }
            active(_t) { height: 26.0 }
        }
        transitions { height: 180ms EaseOut }
    }
}

// A space button. Rounds from circle to squircle on hover/selection —
// the whole animation vocabulary of the rail, and it costs one property.
stylesheet! {
    pub RailButton<IdeaThemeRef> {
        base(t) {
            width: 44.0,
            height: 44.0,
            flex_shrink: 0.0,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: Length::Full,
            background: t.color.surface_alt(),
            color: t.color.text_muted(),
            font_size: 13.0,
            font_weight: FontWeight::Bold,
            letter_spacing: 0.3,
            text_align: TextAlign::Center,
        }
        variant active {
            #[default]
            off(_t) {}
            on(t) {
                border_radius: 14.0,
                background: t.intent.primary.solid_bg(),
                color: t.intent.primary.solid_text(),
            }
        }
        state hovered(t) {
            border_radius: 14.0,
            color: t.color.text(),
        }
        transitions {
            border_radius: 180ms EaseOut,
            background: 180ms EaseOut,
            color: 180ms EaseOut,
        }
    }
}

// "Join or start a space" — a ghost of a space button.
//
// The design draws this dashed. `StyleRules` has no `border_style`, so
// it reads as a hairline outline instead; see the framework findings in
// README.md. The affordance survives — this is the only outlined thing
// in the rail either way.
stylesheet! {
    pub RailAdd<IdeaThemeRef> {
        base(t) {
            width: 44.0,
            height: 44.0,
            flex_shrink: 0.0,
            margin_top: 6.0,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: 14.0,
            border_width: 1.0,
            border_color: t.color.border_strong(),
            color: t.color.text_muted(),
        }
        state hovered(t) {
            background: t.color.surface_alt(),
            color: t.color.text(),
        }
        transitions { background: 150ms EaseOut, color: 150ms EaseOut }
    }
}

// The hairline between "home" and the space list.
stylesheet! {
    pub RailRule<IdeaThemeRef> {
        base(t) {
            width: 22.0,
            height: 1.0,
            flex_shrink: 0.0,
            margin_vertical: 8.0,
            background: t.color.border(),
        }
    }
}

// =============================================================================
// Room list — the 246px middle column
// =============================================================================

stylesheet! {
    pub RoomList<IdeaThemeRef> {
        base(t) {
            width: 246.0,
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            background: t.color.surface(),
            border_right_width: 1.0,
            border_right_color: t.color.border(),
        }
        transitions { background: 220ms EaseInOut, border_right_color: 220ms EaseInOut }
    }
}

// Space name + meta line at the top of the room list.
stylesheet! {
    pub RoomListHead<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            gap: 1.0,
            padding_horizontal: t.spacing.lg(),
            padding_top: t.spacing.lg(),
            padding_bottom: t.spacing.md(),
            flex_shrink: 0.0,
        }
    }
}

// The scrolling body of the room list.
stylesheet! {
    pub RoomListBody<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            flex_direction: FlexDirection::Column,
            width: Length::pct(100.0),
        }
    }
}

stylesheet! {
    pub RoomListPad<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            padding_horizontal: t.spacing.sm(),
            padding_bottom: t.spacing.md(),
        }
    }
}

// "TALK" / "ROOMS" / "CONVERSATIONS" — the small caps section labels.
stylesheet! {
    pub GroupLabel<IdeaThemeRef> {
        base(t) {
            color: t.color.text_muted(),
            font_size: 11.0,
            font_weight: FontWeight::SemiBold,
            letter_spacing: 1.0,
            text_transform: TextTransform::Uppercase,
            text_align: TextAlign::Left,
            padding_horizontal: t.spacing.sm(),
            padding_top: t.spacing.sm(),
            padding_bottom: 6.0,
        }
    }
}

// A channel or room row. The active row is a wash rather than an accent
// fill — with a dozen rows visible, accent on the selection is a
// shouting match.
stylesheet! {
    pub RoomRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 9.0,
            padding_vertical: 7.0,
            padding_horizontal: t.spacing.sm(),
            border_radius: t.radius.md(),
            margin_bottom: 1.0,
            color: t.color.text_muted(),
        }
        variant active {
            #[default]
            off(_t) {}
            on(t) {
                background: t.color.surface_alt(),
                color: t.color.text(),
            }
        }
        state hovered(t) {
            background: t.color.surface_alt(),
            color: t.color.text(),
        }
        transitions { background: 130ms EaseOut, color: 130ms EaseOut }
    }
}

// The row's label. Truncates rather than wrapping — a channel name that
// wraps to two lines breaks the rhythm of the whole list.
stylesheet! {
    pub RoomRowName<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            text_align: TextAlign::Left,
            overflow: Overflow::Hidden,
        }
    }
}

// The unread pip on a channel or conversation row.
//
// Always mounted; `shown` is what reveals it. Adding and removing the node
// would make "is this row selected" a structural question and force the whole
// list to rebuild on every navigation — as a variant it is a re-style of one
// node instead.
stylesheet! {
    pub UnreadDot<IdeaThemeRef> {
        base(t) {
            width: 7.0,
            height: 7.0,
            flex_shrink: 0.0,
            border_radius: Length::Full,
            background: t.intent.primary.solid_bg(),
        }
        variant shown {
            #[default]
            off(_t) { width: 0.0, height: 0.0, opacity: 0.0 }
            on(_t) {}
        }
    }
}

// A face nested under a live room, indented past the room's icon.
stylesheet! {
    pub RoomFace<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 8.0,
            padding_vertical: 3.0,
            padding_right: t.spacing.sm(),
            padding_left: 28.0,
            border_radius: t.radius.md(),
            color: t.color.text_muted(),
            font_size: 13.0,
        }
        state hovered(t) {
            background: t.color.surface_alt(),
            color: t.color.text(),
        }
        transitions { background: 130ms EaseOut }
    }
}

// A DM row: avatar, name, preview, timestamp.
stylesheet! {
    pub DmRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 10.0,
            padding_vertical: 8.0,
            padding_horizontal: t.spacing.sm(),
            border_radius: t.radius.md(),
            margin_bottom: 1.0,
        }
        variant active {
            #[default]
            off(_t) {}
            on(t) { background: t.color.surface_alt() }
        }
        state hovered(t) { background: t.color.surface_alt() }
        transitions { background: 130ms EaseOut }
    }
}

// =============================================================================
// Presence
// =============================================================================

// The avatar's presence dot. Sits on the avatar's corner with a ring in
// the panel colour so it reads as a badge rather than a smudge.
stylesheet! {
    pub PresenceDot<IdeaThemeRef> {
        base(t) {
            position: Position::Absolute,
            right: -2.0,
            bottom: -2.0,
            width: 10.0,
            height: 10.0,
            border_radius: Length::Full,
            border_width: 2.0,
            border_color: t.color.surface(),
            background: t.color.border_strong(),
        }
        variant status {
            #[default]
            offline(_t) {}
            online(t) { background: t.intent.success.fg() }
            away(t) { background: t.intent.warning.fg() }
        }
        variant size {
            #[default]
            md(_t) {}
            sm(_t) { width: 9.0, height: 9.0 }
        }
    }
}

// Wrapper that anchors a presence dot to an avatar.
stylesheet! {
    pub AvatarSlot<()> {
        base(_t) {
            position: Position::Relative,
            flex_shrink: 0.0,
        }
    }
}

// =============================================================================
// Self / voice tray, bottom of the room list
// =============================================================================

stylesheet! {
    pub Tray<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            flex_shrink: 0.0,
            border_top_width: 1.0,
            border_top_color: t.color.border(),
        }
    }
}

// The "you're in The Porch" strip. Green because it is the one piece of
// state in the app that is genuinely live.
stylesheet! {
    pub VoiceStrip<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 8.0,
            padding_vertical: 10.0,
            padding_horizontal: t.spacing.md(),
            background: t.intent.success.soft_bg(),
        }
    }
}

// One bar of the three-bar "audio is flowing" indicator.
stylesheet! {
    pub LevelBar<IdeaThemeRef> {
        base(t) {
            width: 3.0,
            height: 14.0,
            border_radius: 2.0,
            background: t.intent.success.fg(),
        }
        variant step {
            #[default]
            a(_t) { height: 6.0 }
            b(_t) { height: 14.0 }
            c(_t) { height: 9.0 }
        }
    }
}

stylesheet! {
    pub TrayRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 9.0,
            padding_vertical: 10.0,
            padding_horizontal: t.spacing.md(),
        }
    }
}

// =============================================================================
// Main column
// =============================================================================

stylesheet! {
    pub Main<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            // Explicit, not inherited from a flex parent. `AppShell`'s content
            // wrapper is a plain block — it sets height and the pinned-sidebar
            // margin but never `display: flex` — so `flex_grow` alone has no
            // flex context here and the column collapses to its header.
            height: Length::pct(100.0),
            min_height: 0.0,
            flex_direction: FlexDirection::Column,
            position: Position::Relative,
        }
    }
}

// The 56px title bar over the main column.
// The title block inside the header. `min_width: 0` is what lets it shrink
// below its content instead of pushing the search and roster buttons off the
// edge — without it a long channel name wraps the whole bar onto three lines.
stylesheet! {
    pub HeaderTitle<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Baseline,
            gap: 9.0,
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            overflow: Overflow::Hidden,
        }
    }
}

stylesheet! {
    pub Header<IdeaThemeRef> {
        base(t) {
            height: 56.0,
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: t.spacing.sm(),
            overflow: Overflow::Hidden,
            padding_horizontal: t.spacing.sm(),
            border_bottom_width: 1.0,
            border_bottom_color: t.color.border(),
            background: t.color.background(),
        }
        breakpoint md(t) { padding_horizontal: t.spacing.lg() }
        transitions { background: 220ms EaseInOut, border_bottom_color: 220ms EaseInOut }
    }
}

stylesheet! {
    pub Row<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
        }
    }
}

// Row that lets its children keep their intrinsic size and pushes the
// rest away — the `flex: 1` spacer, as a style.
stylesheet! {
    pub Spring<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
        }
    }
}

stylesheet! {
    pub Body<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_height: 0.0,
            flex_direction: FlexDirection::Row,
        }
    }
}

// =============================================================================
// Chat
// =============================================================================

stylesheet! {
    pub Chat<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            // Explicit for the same reason as `Main`: on a narrow screen this
            // sits inside a `Drawer`, whose content wrapper is a plain block,
            // so `flex_grow` has nothing to grow inside and the transcript
            // collapses under the composer.
            height: Length::pct(100.0),
            min_height: 0.0,
            flex_direction: FlexDirection::Column,
        }
    }
}

stylesheet! {
    pub Transcript<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            width: Length::pct(100.0),
            flex_direction: FlexDirection::Column,
            padding_vertical: t.spacing.lg(),
        }
    }
}

// The "this is the top of the channel" block.
stylesheet! {
    pub Intro<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            gap: 6.0,
            max_width: 900.0,
            padding_horizontal: t.spacing.xl(),
            padding_bottom: t.spacing.xl(),
        }
    }
}

// A message row. Hovering washes the whole row, which is what makes the
// hover-only timestamp on a grouped message legible.
stylesheet! {
    pub MessageRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            gap: t.spacing.md(),
            padding_horizontal: t.spacing.xl(),
            position: Position::Relative,
        }
        variant density {
            #[default]
            head(_t) { padding_top: 10.0, padding_bottom: 3.0 }
            sub(_t) { padding_vertical: 2.0 }
            tight(_t) { padding_vertical: 2.0 }
        }
        state hovered(t) { background: t.color.surface_alt() }
        transitions { background: 110ms EaseOut }
    }
}

// The 32px gutter holding either the author's avatar or, on a grouped
// message, the hover timestamp.
stylesheet! {
    pub Gutter<()> {
        base(_t) {
            width: 32.0,
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
        }
    }
}

stylesheet! {
    pub MessageBody<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            flex_direction: FlexDirection::Column,
        }
    }
}

// Author + timestamp, baseline-aligned so the small time sits on the
// name's baseline rather than floating in the middle of it.
stylesheet! {
    pub MessageHead<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Baseline,
            gap: 9.0,
            margin_bottom: 1.0,
        }
    }
}

stylesheet! {
    pub AuthorName<IdeaThemeRef> {
        base(t) {
            // Hugs its text: a name that stretched the column would put its
            // hover affordance under empty space to the right of itself.
            align_self: AlignSelf::FlexStart,
            color: t.color.text(),
            font_weight: FontWeight::SemiBold,
            text_align: TextAlign::Left,
        }
        state hovered(t) {
            color: t.intent.primary.fg(),
        }
        transitions { color: 120ms EaseOut }
    }
}

// The message text itself.
stylesheet! {
    pub MessageText<IdeaThemeRef> {
        base(t) {
            color: t.color.text(),
            text_align: TextAlign::Left,
            line_height: 21.0,
        }
        variant muted {
            #[default]
            off(_t) {}
            // A muted person's messages stay readable but recede — hiding
            // them entirely leaves holes in a conversation that other people
            // are still replying to.
            on(t) { color: t.color.text_muted() }
        }
    }
}

// The floating action cluster on a hovered message.
stylesheet! {
    pub MessageActions<IdeaThemeRef> {
        base(t) {
            position: Position::Absolute,
            right: 24.0,
            top: -14.0,
            flex_direction: FlexDirection::Row,
            gap: 2.0,
            padding: 3.0,
            border_radius: t.radius.md(),
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
            opacity: 0.0,
        }
        variant shown {
            #[default]
            off(_t) {}
            on(_t) { opacity: 1.0 }
        }
        transitions { opacity: 120ms EaseOut }
    }
}

// =============================================================================
// Composer
// =============================================================================

stylesheet! {
    pub ComposerWrap<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            flex_shrink: 0.0,
            padding_horizontal: t.spacing.xl(),
            padding_bottom: t.spacing.lg(),
        }
    }
}

// The composer box. Its border is the focus affordance — the one place
// in the app where focus is worth drawing, because it is the one place
// you type.
stylesheet! {
    pub Composer<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::FlexEnd,
            gap: 8.0,
            padding_top: 8.0,
            padding_right: 8.0,
            padding_bottom: 8.0,
            padding_left: t.spacing.md(),
            border_radius: t.radius.lg(),
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
        variant focused {
            #[default]
            off(_t) {}
            on(t) { border_color: t.color.focus_ring() }
        }
        transitions { border_color: 150ms EaseOut }
    }
}

// The text field inside the composer: no chrome of its own, because the
// box around it already is the chrome.
stylesheet! {
    pub ComposerInput<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            background: Color("transparent".into()),
            color: t.color.text(),
            font_size: 15.0,
            padding_vertical: 6.0,
            border_width: 0.0,
        }
    }
}

// The two-line footnote under the composer.
stylesheet! {
    pub ComposerHint<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            gap: t.spacing.md(),
            padding_top: 7.0,
            padding_horizontal: 4.0,
            color: t.color.text_muted(),
            font_size: 11.0,
        }
    }
}

// =============================================================================
// People sidebar
// =============================================================================

stylesheet! {
    pub People<IdeaThemeRef> {
        base(t) {
            width: 224.0,
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            border_left_width: 1.0,
            border_left_color: t.color.border(),
            background: t.color.surface(),
        }
        transitions { background: 220ms EaseInOut }
    }
}

stylesheet! {
    pub PeoplePad<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            padding_vertical: t.spacing.lg(),
            padding_horizontal: t.spacing.md(),
        }
    }
}

stylesheet! {
    pub PersonRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 9.0,
            padding_vertical: 5.0,
            padding_horizontal: t.spacing.sm(),
            border_radius: t.radius.md(),
        }
        variant present {
            #[default]
            here(_t) {}
            // Offline people stay listed but recede: knowing who *isn't*
            // around is worth a row, just not a bright one.
            gone(_t) { opacity: 0.5 }
        }
        state hovered(t) { background: t.color.surface_alt() }
        transitions { background: 130ms EaseOut }
    }
}

// =============================================================================
// Live rooms
// =============================================================================

stylesheet! {
    pub Live<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            flex_direction: FlexDirection::Column,
            background: t.color.surface_alt(),
        }
        transitions { background: 220ms EaseInOut }
    }
}

// A person's tile in a room. The ring is the speaking indicator; it's a
// border rather than a glow so it survives on every backend.
stylesheet! {
    pub Tile<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            height: 168.0,
            border_radius: t.radius.lg(),
            background: t.color.surface(),
            border_width: 2.0,
            border_color: t.color.border(),
            position: Position::Relative,
            overflow: Overflow::Hidden,
        }
        variant speaking {
            #[default]
            off(_t) {}
            on(t) { border_color: t.intent.success.fg() }
        }
        variant you {
            #[default]
            off(_t) {}
            on(t) { border_color: t.intent.primary.border() }
        }
        transitions { border_color: 200ms EaseOut }
    }
}

// The name plate in a tile's bottom-left corner.
stylesheet! {
    pub TileName<()> {
        base(_t) {
            position: Position::Absolute,
            left: 12.0,
            bottom: 11.0,
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 6.0,
        }
    }
}

// The "invite someone" tile — same footprint, outlined rather than
// filled. Dashed in the design; `StyleRules` has no `border_style`.
stylesheet! {
    pub TileGhost<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            gap: 8.0,
            height: 168.0,
            border_radius: t.radius.lg(),
            border_width: 2.0,
            border_color: t.color.border_hover(),
            color: t.color.text_muted(),
        }
        state hovered(t) {
            border_color: t.color.border_strong(),
            color: t.color.text(),
        }
        transitions { border_color: 150ms EaseOut, color: 150ms EaseOut }
    }
}

// The screen-share stage.
stylesheet! {
    pub Stage<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            flex_direction: FlexDirection::Column,
            border_radius: t.radius.lg(),
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
            overflow: Overflow::Hidden,
        }
    }
}

stylesheet! {
    pub StageBar<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 8.0,
            padding_vertical: 10.0,
            padding_horizontal: t.spacing.md(),
            border_bottom_width: 1.0,
            border_bottom_color: t.color.border(),
            flex_shrink: 0.0,
        }
    }
}

stylesheet! {
    pub StageSurface<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            gap: 10.0,
            background: t.color.background(),
            color: t.color.text_muted(),
        }
    }
}

// The recording pip beside "Ana is sharing".
stylesheet! {
    pub LiveDot<IdeaThemeRef> {
        base(t) {
            width: 8.0,
            height: 8.0,
            flex_shrink: 0.0,
            border_radius: Length::Full,
            background: t.intent.danger.fg(),
        }
    }
}

// The floating call controls.
stylesheet! {
    pub CallBar<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            align_self: AlignSelf::Center,
            gap: 8.0,
            padding: 8.0,
            margin_vertical: t.spacing.md(),
            border_radius: t.radius.lg(),
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

stylesheet! {
    pub CallBarRule<IdeaThemeRef> {
        base(t) {
            width: 1.0,
            height: 28.0,
            margin_horizontal: 4.0,
            background: t.color.border(),
        }
    }
}

// =============================================================================
// Settings
// =============================================================================

stylesheet! {
    pub SettingsPage<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            width: Length::pct(100.0),
            flex_direction: FlexDirection::Column,
            padding_vertical: t.spacing.xxl(),
            padding_horizontal: t.spacing.xl(),
        }
    }
}

stylesheet! {
    pub SettingsColumn<IdeaThemeRef> {
        base(t) {
            width: Length::pct(100.0),
            max_width: 640.0,
            align_self: AlignSelf::Center,
            flex_direction: FlexDirection::Column,
            gap: t.spacing.xl(),
        }
    }
}

stylesheet! {
    pub SettingsGroup<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
            border_radius: t.radius.lg(),
            overflow: Overflow::Hidden,
        }
        transitions { background: 220ms EaseInOut }
    }
}

stylesheet! {
    pub SettingsRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: t.spacing.lg(),
            padding_vertical: t.spacing.md(),
            padding_horizontal: t.spacing.lg(),
            border_top_width: 1.0,
            border_top_color: t.color.border(),
        }
    }
}

// =============================================================================
// Overlays — thread, search, menus, cards
// =============================================================================

// The thread panel. Mobile-first: it covers the column, because a 380px dock
// beside a phone-width conversation leaves neither readable. From `md` up it
// becomes the dock the design draws, over the main column's right edge.
stylesheet! {
    pub ThreadPanel<IdeaThemeRef> {
        base(t) {
            position: Position::Absolute,
            top: 56.0,
            right: 0.0,
            bottom: 0.0,
            width: Length::pct(100.0),
            flex_direction: FlexDirection::Column,
            background: t.color.surface(),
            border_left_width: 1.0,
            border_left_color: t.color.border(),
        }
        breakpoint md(_t) {
            width: 380.0,
        }
    }
}

stylesheet! {
    pub PanelHead<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: t.spacing.sm(),
            padding_vertical: t.spacing.md(),
            padding_horizontal: t.spacing.lg(),
            border_bottom_width: 1.0,
            border_bottom_color: t.color.border(),
            flex_shrink: 0.0,
        }
    }
}

// A full-window scrim. Overlays that take the whole screen sit on this.
stylesheet! {
    pub Scrim<IdeaThemeRef> {
        base(t) {
            position: Position::Absolute,
            top: 0.0, left: 0.0, right: 0.0, bottom: 0.0,
            background: t.color.overlay(),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            padding: t.spacing.xl(),
        }
        // A near-black hold-out for the device preview. The one literal colour
        // in this file, and deliberate: `color-overlay` is tuned to dim a
        // *page* and barely registers over Hearth's own near-black dark
        // surfaces, whereas this scrim's job is to remove the desktop from
        // view entirely so the phone reads as a separate object. It must be
        // the same darkness in both schemes, so there is no token for it.
        variant depth {
            #[default]
            page(_t) {}
            deep(_t) { background: Color("rgba(6, 10, 18, 0.86)".into()) }
        }
    }
}

// Backdrop press target — fills the scrim behind the panel so a click
// anywhere outside dismisses.
stylesheet! {
    pub ScrimCatcher<()> {
        base(_t) {
            position: Position::Absolute,
            top: 0.0, left: 0.0, right: 0.0, bottom: 0.0,
        }
    }
}

// A floating card: search results, the join dialog, a profile.
stylesheet! {
    pub Panel<IdeaThemeRef> {
        base(t) {
            position: Position::Relative,
            flex_direction: FlexDirection::Column,
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
            border_radius: t.radius.lg(),
            overflow: Overflow::Hidden,
        }
    }
}

// The search field's row.
stylesheet! {
    pub SearchBar<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 10.0,
            padding_vertical: t.spacing.md(),
            padding_horizontal: t.spacing.lg(),
            border_bottom_width: 1.0,
            border_bottom_color: t.color.border(),
            flex_shrink: 0.0,
        }
    }
}

// A bare input: the surrounding row owns the chrome.
stylesheet! {
    pub BareInput<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            background: Color("transparent".into()),
            color: t.color.text(),
            border_width: 0.0,
            font_size: 18.0,
        }
    }
}

// The `esc` key cap in the search bar.
stylesheet! {
    pub KeyCap<IdeaThemeRef> {
        base(t) {
            color: t.color.text_muted(),
            font_size: 11.0,
            border_width: 1.0,
            border_color: t.color.border(),
            border_radius: t.radius.sm(),
            padding_vertical: 2.0,
            padding_horizontal: 6.0,
            flex_shrink: 0.0,
        }
    }
}

stylesheet! {
    pub ResultRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            gap: 10.0,
            padding_vertical: 10.0,
            padding_horizontal: t.spacing.md(),
            border_radius: t.radius.md(),
        }
        state hovered(t) { background: t.color.surface_alt() }
        transitions { background: 130ms EaseOut }
    }
}

// The person context menu. Positioned at the pointer, so it carries no
// layout of its own beyond its own width.
stylesheet! {
    pub ContextMenu<IdeaThemeRef> {
        base(t) {
            position: Position::Absolute,
            width: 236.0,
            padding: 5.0,
            flex_direction: FlexDirection::Column,
            border_radius: t.radius.lg(),
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

stylesheet! {
    pub MenuItem<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 8.0,
            padding_vertical: 8.0,
            padding_horizontal: t.spacing.sm(),
            border_radius: t.radius.sm(),
            color: t.color.text(),
        }
        variant kind {
            #[default]
            normal(_t) {}
            danger(t) { color: t.intent.danger.fg() }
        }
        variant divided {
            #[default]
            off(_t) {}
            on(t) {
                margin_top: 5.0,
                border_top_width: 1.0,
                border_top_color: t.color.border(),
            }
        }
        state hovered(t) { background: t.color.surface_alt() }
        transitions { background: 110ms EaseOut }
    }
}

// A padded section inside a floating card, ruled off from the one above.
stylesheet! {
    pub CardSection<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            gap: 10.0,
            padding_vertical: t.spacing.md(),
            padding_horizontal: t.spacing.lg(),
            border_top_width: 1.0,
            border_top_color: t.color.border(),
        }
    }
}

stylesheet! {
    pub WrapRow<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            flex_wrap: runtime_core::FlexWrap::Wrap,
            gap: 6.0,
            align_items: AlignItems::Center,
        }
    }
}

// =============================================================================
// Small shared bits
// =============================================================================

// A column that takes the leftover width and clips rather than pushing its
// row wider: a name over a note, a title over a preview.
//
// `flex_direction` is explicit. The primitive's default is Row here, so
// without it a name and its note render side by side and run together.
stylesheet! {
    pub Ellipsis<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            overflow: Overflow::Hidden,
            text_align: TextAlign::Left,
        }
    }
}

// A press target that adds nothing visually — used to make a plain
// layout box clickable without inheriting button chrome. Stretches, so it can
// wrap a full-width row.
stylesheet! {
    pub Bare<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
            background: Color("transparent".into()),
            border_width: 0.0,
            padding: 0.0,
        }
    }
}

// `Bare`, but hugging its content instead of filling the row — for press
// targets that ARE the thing pressed (an author's name, a thread badge)
// rather than a row containing it. Without this the badge stretches the full
// column width and its label centres itself in the middle of the message.
stylesheet! {
    pub Hug<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_self: AlignSelf::FlexStart,
            background: Color("transparent".into()),
            border_width: 0.0,
            padding: 0.0,
        }
    }
}

// A plain vertical stack.
//
// A bare `view` is `display: block`, so two text children flow INLINE and run
// together on one line — a title and its subtitle end up as "SettingsSeven of
// them." Anywhere text stacks, the container has to say so.
stylesheet! {
    pub Column<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
        }
    }
}

// The search palette's result list.
//
// Sizes to its content and caps, rather than `flex-grow`ing: the modal body
// around it is content-sized, so a `flex-basis: 0` child contributes nothing
// to that height and then grows into zero free space — the list disappears
// entirely, empty state and all.
stylesheet! {
    pub ResultsScroll<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
            width: Length::pct(100.0),
            max_height: 380.0,
        }
    }
}

// ===========================================================================
// Auth
// ===========================================================================

// The full-window ground behind a sign-in card.
stylesheet! {
    pub AuthPage<IdeaThemeRef> {
        base(t) {
            width: Length::pct(100.0),
            height: Length::pct(100.0),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            padding: t.spacing.xl(),
            background: t.color.background(),
            color: t.color.text(),
        }
        transitions { background: 220ms EaseInOut }
    }
}

// One centred card. Capped rather than stretched: a sign-in form the width of
// a monitor is a worse form, and the cap is what makes the same layout work on
// a phone.
stylesheet! {
    pub AuthCard<IdeaThemeRef> {
        base(t) {
            width: Length::pct(100.0),
            max_width: 420.0,
            flex_direction: FlexDirection::Column,
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
            border_radius: t.radius.lg(),
            overflow: Overflow::Hidden,
        }
        transitions { background: 220ms EaseInOut }
    }
}

// The message under a form. Danger-toned because in practice it is almost
// always a refusal, and a neutral one reads as decoration.
stylesheet! {
    pub Notice<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            padding_vertical: t.spacing.sm(),
            padding_horizontal: t.spacing.md(),
            border_radius: t.radius.md(),
            background: t.intent.danger.soft_bg(),
            color: t.intent.danger.soft_text(),
        }
    }
}

// A copyable value handed back to the user — an invite code.
stylesheet! {
    pub CodeBox<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: t.spacing.sm(),
            padding_vertical: t.spacing.sm(),
            padding_horizontal: t.spacing.md(),
            border_radius: t.radius.md(),
            background: t.color.surface_alt(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
    }
}
