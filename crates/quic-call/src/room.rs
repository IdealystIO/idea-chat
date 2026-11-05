//! The call's room: everyone as a tile, laid out as a grid with an overflow
//! column.
//!
//! - **A person** is one tile: their camera when it is on, otherwise a card
//!   with their avatar — audio with no video of its own.
//! - **A screen share** is a tile of its own, first in the order, because it
//!   is usually what everyone is looking at.
//! - **Up to [`GRID_MAX`] tiles** share the grid, in as square an arrangement
//!   as the count allows; anyone past that goes in a narrow column beside it.
//! - **Clicking a tile focuses it**: it takes the whole main area and every
//!   other tile moves to the side column. Clicking it again goes back to the
//!   grid; so does the focused tile going away (its person leaving, its share
//!   ending), because a focus on a key that is no longer in the room is
//!   simply not applied.
//! - **Each remote tile has a menu**, from a button in its corner and anchored
//!   to it (Hearth's person-menu pattern): focus the tile, our volume on that
//!   person's voice or that screen's audio (0–200%, through `voice_dsp::taper`,
//!   so 100% is exactly as sent), and mute it for us. Muting keeps the volume,
//!   so unmuting brings them back where they were; a muted tile says so on its
//!   name. The menu stays open across a mute, its rows following.
//! - **Everyone's own mute shows** as a mic-off mark on their name — ours from
//!   the button, theirs as they announce it. All of it is ours alone — nobody else hears it or is told. The
//!   button sits outside the tile's press area, so opening the menu does not
//!   also focus the tile.
//!
//! - **Tiles fade in** when they arrive — someone joins, a share starts — and
//!   when one is newly focused; a rebuild that only moves tiles (a mute, a
//!   camera turning on) does not replay it. The speaking ring and the hover
//!   outline ease rather than snap.
//!
//! The grid FITS the space it is given — rows share the height, cells share
//! a row's width — rather than sizing tiles by width and letting the rows run
//! past the window; video fills each cell (cameras cropped to it, screens
//! letterboxed in it). Hand-built rather than idea-ui's `Grid` because that
//! divides width only, and a call has to fit between its status line and its
//! control bar. The side column keeps 16:9 tiles and scrolls.
//!
//! Platform-free: the tiles' video players are built by the caller (they are
//! web-only) and handed in, so the layout itself — and its arithmetic — is
//! testable natively.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use icons_lucide::{ELLIPSIS_VERTICAL, MAXIMIZE_2, MIC_OFF, MINIMIZE_2, SHIELD_ALERT, VOLUME_2, VOLUME_X};
use idea_ui::{
    tone, typography_kind, variant, Avatar, AvatarSize, ToneRef, ControlSize, ElementAlign, Icon, IconButton,
    IconButtonSize, IdeaThemeRef, Menu, MenuItem, MenuLabel, MenuSeparator, Slider, Tooltip, Typography,
};
use runtime_core::accessibility::Role;
use runtime_core::primitives::portal::AnchorTarget;
use runtime_core::{
    pressable, rx, stylesheet, switch, ui, AlignItems, Cursor, Element, FlexDirection, JustifyContent, Length,
    Overflow, Position, Ref, Signal, StyleApplication, Transform, ViewHandle,
};

/// The most a volume slider goes to: 200%, which `voice_dsp::taper` makes
/// 4× — `voice_dsp::MAX_GAIN`, +12 dB. Hearth's range too.
pub const VOLUME_MAX: f32 = 2.0;

/// Everything the room reads and writes besides its tiles.
#[derive(Clone)]
pub struct Controls {
    /// Ids heard speaking now, [`ME`] for us.
    pub speaking: Signal<Vec<u32>>,
    /// The focused tile's key, if any.
    pub focus: Signal<Option<String>>,
    /// Slider positions by tile key (1.0 = 100%); a missing key is 100%.
    pub volumes: Signal<BTreeMap<String, f32>>,
    /// Tiles we have muted for ourselves, by key. Their volume is kept.
    pub silenced: Signal<BTreeSet<String>>,
    /// The tile whose menu is open.
    pub menu: Signal<Option<String>>,
    /// Called with a tile key and the slider position to play it at: its
    /// volume, or 0 while it is muted.
    pub on_volume: Rc<dyn Fn(String, f32)>,
    /// Tiles that have finished fading in, by key. A tile not in here when
    /// the room is built is new, and fades in.
    pub entered: Signal<BTreeSet<String>>,
    /// The focus the room was last built with, so a newly focused tile fades
    /// in too and a rebuild for anything else leaves it alone.
    pub last_focus: Rc<RefCell<Option<String>>>,
}

/// The most tiles the grid holds before the rest go to the side column. Nine
/// is a 3×3 — past that, faces get too small to read.
pub const GRID_MAX: usize = 9;

/// Stands in for "me" in the speaking set, which is otherwise relay ids.
pub const ME: u32 = u32::MAX;

/// Which tiles go in the main area and which in the side column: the
/// focused one alone when it is present, else the first [`GRID_MAX`].
/// Returns indices into `keys`, in order.
pub fn arrange(keys: &[&str], focus: Option<&str>) -> (Vec<usize>, Vec<usize>) {
    match focus.and_then(|f| keys.iter().position(|k| *k == f)) {
        Some(i) => (vec![i], (0..keys.len()).filter(|&j| j != i).collect()),
        None => {
            let (grid, _) = split(keys.len());
            ((0..grid).collect(), (grid..keys.len()).collect())
        }
    }
}

/// How many tiles go in the grid, and in how many columns.
pub fn split(tiles: usize) -> (usize, u32) {
    let grid = tiles.min(GRID_MAX);
    let columns = match grid {
        0 | 1 => 1,
        2..=4 => 2,
        _ => 3,
    };
    (grid, columns)
}

/// One tile's worth of what to show.
pub struct Tile {
    /// Stable across camera and mute changes — what focus remembers.
    pub key: String,
    /// The person, for the speaking ring and the avatar's colour. [`ME`] for us.
    pub id: u32,
    pub label: String,
    /// A playing video, or `None` for an avatar card.
    pub video: Option<Element>,
    pub screen: bool,
    /// Whether their mic is muted: read live, so a mute redraws the mark and
    /// not the tile (whose video would restart). `None` for tiles with no mic
    /// of their own — screens.
    pub muted: Option<Rc<dyn Fn() -> bool>>,
    /// Whether their media fails to open with our end-to-end key — a
    /// different key, or none — read live like `muted`. `None` where it
    /// cannot happen.
    pub unreadable: Option<Rc<dyn Fn() -> bool>>,
    /// Whether the tile has a menu (focus, volume, mute) — everyone's but our
    /// own.
    pub volume: bool,
}

stylesheet! {
    pub Room<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            flex_grow: 1.0,
            gap: 12.0,
            min_height: 0.0,
        }
    }
}

stylesheet! {
    pub Main<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_width: 0.0,
            min_height: 0.0,
            flex_direction: FlexDirection::Column,
            gap: 12.0,
        }
    }
}

stylesheet! {
    /// A grid row: an equal share of the height.
    pub Row<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
            min_height: 0.0,
            flex_direction: FlexDirection::Row,
            gap: 12.0,
        }
    }
}

stylesheet! {
    /// Pads a short last row, so its tiles keep the others' width.
    pub Filler<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_basis: 0.0,
        }
    }
}

stylesheet! {
    /// The overflow column: narrow, and scrolls when even it is full.
    pub Side<()> {
        base(_t) {
            width: 220.0,
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            gap: 8.0,
            max_height: Length::pct(100.0),
        }
    }
}

stylesheet! {
    /// A tile. The ring is the speaking indicator — a border rather than a
    /// glow, so it survives on every backend (Hearth's convention).
    pub Cell<IdeaThemeRef> {
        base(t) {
            position: Position::Relative,
            border_radius: t.radius.md(),
            border_width: 2.0,
            border_color: t.color.border(),
            background: t.color.surface_alt(),
            overflow: Overflow::Hidden,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
        }
        state hovered(t) {
            border_color: t.color.border_strong(),
        }
        variant speaking {
            #[default]
            off(_t) {}
            on(t) { border_color: t.intent.success.fg() }
        }
        // A new tile starts small and clear and eases to rest.
        variant shown {
            #[default]
            on(_t) {}
            off(_t) { opacity: 0.0, transform: vec![Transform::Scale(0.94)] }
        }
        // In the grid a tile takes its share of the row; in the side column
        // it keeps a 16:9 shape at the column's width.
        variant place {
            #[default]
            grid(_t) { flex_grow: 1.0, flex_basis: 0.0, min_width: 0.0, height: Length::pct(100.0) }
            side(_t) { width: Length::pct(100.0), aspect_ratio: 16.0 / 9.0, flex_shrink: 0.0 }
        }
        transitions {
            border_color: 180ms EaseOut,
            opacity: 260ms EaseOut,
            transform: 260ms EaseOut,
        }
    }
}

stylesheet! {
    /// The press area: the whole tile under the corner button.
    pub Press<()> {
        base(_t) {
            position: Position::Absolute,
            left: 0.0,
            top: 0.0,
            width: Length::pct(100.0),
            height: Length::pct(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            cursor: Cursor::Pointer,
        }
    }
}

stylesheet! {
    pub Corner<()> {
        base(_t) {
            position: Position::Absolute,
            top: 6.0,
            right: 6.0,
        }
    }
}

stylesheet! {
    /// Where the menu region sits: nowhere, taking no space — the menu itself
    /// is portalled and anchored to the tile's corner button.
    pub Nowhere<()> {
        base(_t) {
            position: Position::Absolute,
            left: 0.0,
            top: 0.0,
        }
    }
}

stylesheet! {
    pub VolumeRow<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 8.0,
            padding_left: 12.0,
            padding_right: 12.0,
            padding_top: 6.0,
            padding_bottom: 8.0,
        }
    }
}

stylesheet! {
    pub Name<IdeaThemeRef> {
        base(t) {
            position: Position::Absolute,
            left: 8.0,
            bottom: 8.0,
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 4.0,
            padding_left: 8.0,
            padding_right: 8.0,
            padding_top: 2.0,
            padding_bottom: 2.0,
            border_radius: t.radius.sm(),
            background: t.color.surface(),
        }
    }
}

fn avatar_color(id: u32) -> ToneRef {
    match id % 5 {
        0 => tone::Primary.into(),
        1 => tone::Info.into(),
        2 => tone::Warning.into(),
        3 => tone::Success.into(),
        _ => tone::Danger.into(),
    }
}

fn initials(id: u32) -> String {
    if id == ME {
        "You".to_string()
    } else {
        id.to_string()
    }
}

/// Play `key` at `position`, or at nothing while `muted`. Given both rather
/// than reading them back, because `Signal::set` stages: a read straight
/// after a set still sees the old value.
fn apply(ctl: &Controls, key: &str, position: f32, muted: bool) {
    (ctl.on_volume)(key.to_string(), if muted { 0.0 } else { position });
}

/// One tile's menu, anchored to its corner button.
fn tile_menu(key: String, label: String, anchor: Ref<ViewHandle>, ctl: &Controls) -> Element {
    let (volumes, menu, focus, silenced) = (ctl.volumes, ctl.menu, ctl.focus, ctl.silenced);
    let focused = focus.peek().as_deref() == Some(key.as_str());
    let (k_value, k_readout, k_change, k_focus, k_mute) = (key.clone(), key.clone(), key.clone(), key.clone(), key.clone());
    let (k_heading, k_icon, k_item) = (key.clone(), key.clone(), key);
    let on_change: Rc<dyn Fn(f32)> = {
        let ctl = ctl.clone();
        Rc::new(move |position: f32| {
            // The signal first, so the slider tracks the drag; then the call.
            // While muted this changes nothing audible, and is still right:
            // it is the level they come back at.
            let mut all = volumes.get();
            all.insert(k_change.clone(), position);
            volumes.set(all);
            apply(&ctl, &k_change, position, silenced.peek().contains(&k_change));
        })
    };
    let on_focus: Rc<dyn Fn()> = Rc::new(move || {
        focus.set(if focused { None } else { Some(k_focus.clone()) });
        menu.set(None);
    });
    let on_mute: Rc<dyn Fn()> = {
        let ctl = ctl.clone();
        Rc::new(move || {
            let mut all = silenced.get();
            let now_muted = !all.remove(&k_mute);
            if now_muted {
                all.insert(k_mute.clone());
            }
            silenced.set(all);
            let position = volumes.peek().get(&k_mute).copied().unwrap_or(1.0);
            apply(&ctl, &k_mute, position, now_muted);
            // The menu stays open: its rows say the new state in place, and
            // the volume is right there to adjust.
        })
    };
    let dismiss: Rc<dyn Fn()> = Rc::new(move || menu.set(None));
    let focus_icon: Element = ui! { Icon(data = if focused { MINIMIZE_2 } else { MAXIMIZE_2 }, size = 15.0) };
    let mute_icon = switch(
        move || silenced.get().contains(&k_icon),
        |&muted: &bool| ui! { Icon(data = if muted { VOLUME_2 } else { VOLUME_X }, size = 15.0) },
    );
    ui! {
        Menu(
            target = Some(AnchorTarget::from(anchor)),
            on_dismiss = Some(dismiss),
            align = ElementAlign::End,
        ) {
            MenuLabel(text = label)
            MenuItem(
                label = if focused { "Back to the grid".to_string() } else { "Focus".to_string() },
                leading = Some(focus_icon),
                on_select = on_focus,
            )
            MenuSeparator()
            // Says which state this is in, because the slider alone cannot: 40%
            // and 40% behind a mute look the same, and only one makes a sound.
            MenuLabel(text = rx!(if silenced.get().contains(&k_heading) {
                "Muted for you — volume kept".to_string()
            } else {
                "Volume for you".to_string()
            }))
            view(style = || StyleApplication::new(VolumeRow::sheet())) {
                Slider(
                    value = rx!(volumes.get().get(&k_value).copied().unwrap_or(1.0)),
                    on_change = on_change,
                    min = 0.0,
                    max = VOLUME_MAX,
                    width = 132.0,
                    size = ControlSize::Sm,
                    leading_icon = Some(VOLUME_X),
                    trailing_icon = Some(VOLUME_2),
                )
                Typography(
                    content = rx!(format!("{:.0}%", volumes.get().get(&k_readout).copied().unwrap_or(1.0) * 100.0)),
                    kind = typography_kind::Caption,
                )
            }
            MenuItem(
                label = rx!(if silenced.get().contains(&k_item) {
                    "Unmute for me".to_string()
                } else {
                    "Mute for me".to_string()
                }),
                leading = Some(mute_icon),
                on_select = on_mute,
            )
        }
    }
}

fn cell(tile: Tile, ctl: &Controls, side: bool) -> Element {
    let (speaking, focus) = (ctl.speaking, ctl.focus);
    let id = tile.id;
    let key = tile.key.clone();
    let shown_key = key.clone();
    let fresh = !ctl.entered.peek().contains(&key);
    let menu_label = tile.label.clone();
    let button_label = format!("Options for {}", tile.label);
    let label = tile.label;
    let body: Element = match tile.video {
        Some(video) => video,
        None => ui! {
            Avatar(initials = initials(id), tone = avatar_color(id), size = AvatarSize::Lg)
        },
    };
    // A screen is not someone speaking; only people get the ring.
    let ringed = !tile.screen;
    let press_children: Vec<Element> = vec![
        body,
        {
            let mut name = Vec::new();
            if let Some(unreadable) = tile.unreadable {
                name.push(switch(
                    move || unreadable(),
                    |&on: &bool| {
                        if on {
                            let icon = ui! { Icon(data = SHIELD_ALERT, size = 14.0, tone = Some(tone::Warning.into())) };
                            let mark: Element = runtime_core::view(vec![icon])
                                .a11y_role(Role::Image)
                                .a11y_label("Cannot decrypt")
                                .into();
                            ui! { Tooltip(text = "Cannot decrypt — they are not using this call's key".to_string()) { mark } }
                        } else {
                            runtime_core::fragment(Vec::new())
                        }
                    },
                ));
            }
            if let Some(muted) = tile.muted {
                name.push(switch(
                    move || muted(),
                    |&on: &bool| {
                        if on {
                            let icon = ui! { Icon(data = MIC_OFF, size = 14.0, tone = Some(tone::Danger.into())) };
                            runtime_core::view(vec![icon]).a11y_role(Role::Image).a11y_label("Mic off").into()
                        } else {
                            runtime_core::fragment(Vec::new())
                        }
                    },
                ));
            }
            if tile.volume {
                let (silenced, key) = (ctl.silenced, key.clone());
                name.push(switch(
                    move || silenced.get().contains(&key),
                    |&on: &bool| {
                        if on {
                            ui! { Icon(data = VOLUME_X, size = 14.0, tone = Some(tone::Danger.into())) }
                        } else {
                            runtime_core::fragment(Vec::new())
                        }
                    },
                ));
            }
            name.push(ui! { Typography(content = label, kind = typography_kind::Caption) });
            let pill: Element = runtime_core::view(name).with_style(|| StyleApplication::new(Name::sheet())).into();
            pill
        },
    ];
    // Click to focus; click the focused tile to go back to the grid.
    let focus_key = key.clone();
    let press: Element = pressable(press_children, move || {
        let again = focus.get().as_deref() == Some(focus_key.as_str());
        focus.set(if again { None } else { Some(focus_key.clone()) });
    })
    .with_style(|| StyleApplication::new(Press::sheet()))
    .into();

    let mut children = vec![press];
    if tile.volume {
        let menu = ctl.menu;
        let open_key = key.clone();
        let open: Rc<dyn Fn()> = Rc::new(move || menu.set(Some(open_key.clone())));
        // IconButton takes no accessible name; its wrapper carries one, and is
        // what the menu anchors to.
        let button_anchor: Ref<ViewHandle> = Ref::new();
        let button: Element = runtime_core::view(vec![ui! {
            IconButton(
                glyph = String::new(),
                icon = Some(ELLIPSIS_VERTICAL),
                tone = tone::Neutral,
                variant = variant::Soft,
                size = IconButtonSize::Sm,
                on_click = open,
            )
        }])
        .a11y_role(Role::Group)
        .a11y_label(button_label)
        .bind(button_anchor.clone())
        .into();
        children.push(ui! {
            view(style = || StyleApplication::new(Corner::sheet())) {
                Tooltip(text = "More".to_string()) { button }
            }
        });
        // Only this region rebuilds when the menu opens or closes, so nobody's
        // video restarts for it.
        let (ctl, region_key) = (ctl.clone(), key.clone());
        children.push(switch(
            move || menu.get().as_deref() == Some(region_key.as_str()),
            move |&open: &bool| {
                let inner: Vec<Element> = if open {
                    vec![tile_menu(key.clone(), menu_label.clone(), button_anchor.clone(), &ctl)]
                } else {
                    Vec::new()
                };
                ui! { view(style = || StyleApplication::new(Nowhere::sheet())) { inner } }
            },
        ));
    }
    // A new tile is drawn clear, then marked entered a moment later, which
    // its style eases to — the delay lets the browser paint the start first.
    let entered = ctl.entered;
    if fresh {
        let key = shown_key.clone();
        runtime_core::after_ms_detached(30, move || {
            if entered.is_alive() {
                entered.update(|all| {
                    let mut all = all.clone();
                    all.insert(key);
                    all
                });
            }
        });
    }
    runtime_core::view(children)
        .with_style(move || {
            let on = ringed && speaking.get().contains(&id);
            let shown = entered.get().contains(&shown_key);
            StyleApplication::new(Cell::sheet())
                .with("speaking", if on { "on" } else { "off" })
                .with("place", if side { "side" } else { "grid" })
                .with("shown", if shown { "on" } else { "off" })
        })
        .into()
}

/// The room, from its tiles in display order, with `focus` naming the tile
/// (by [`Tile::key`]) that takes the main area, if any.
pub fn room(tiles: Vec<Tile>, ctl: &Controls) -> Element {
    let focused = ctl.focus.get();
    // Forget who left, so they fade in again if they come back; and a newly
    // focused tile fades in where it lands.
    {
        let present: BTreeSet<String> = tiles.iter().map(|t| t.key.clone()).collect();
        let mut entered: BTreeSet<String> = ctl.entered.peek().intersection(&present).cloned().collect();
        let mut last = ctl.last_focus.borrow_mut();
        if *last != focused {
            if let Some(f) = &focused {
                entered.remove(f);
            }
            *last = focused.clone();
        }
        ctl.entered.set_untracked(entered);
    }
    let (main, rest) = {
        let keys: Vec<&str> = tiles.iter().map(|t| t.key.as_str()).collect();
        arrange(&keys, focused.as_deref())
    };
    let columns = if main.len() == 1 { 1 } else { split(main.len()).1 as usize };
    let mut slots: Vec<Option<Tile>> = tiles.into_iter().map(Some).collect();
    let mut take = |i: usize| slots[i].take().expect("each index once");
    let mut cells: Vec<Element> = main.iter().map(|&i| cell(take(i), ctl, false)).collect();
    // Pad the last row so every tile in the grid is the same size.
    while cells.len() % columns != 0 {
        cells.push(ui! { view(style = || StyleApplication::new(Filler::sheet())) {} });
    }
    let mut rows: Vec<Element> = Vec::new();
    let mut cells = cells.into_iter();
    loop {
        let row: Vec<Element> = cells.by_ref().take(columns).collect();
        if row.is_empty() {
            break;
        }
        rows.push(ui! { view(style = || StyleApplication::new(Row::sheet())) { row } });
    }
    let side: Vec<Element> = rest.iter().map(|&i| cell(take(i), ctl, true)).collect();
    let side: Vec<Element> = if side.is_empty() {
        Vec::new()
    } else {
        vec![ui! {
            scroll_view(style = || StyleApplication::new(Side::sheet())) { side }
        }]
    };
    ui! {
        view(style = || StyleApplication::new(Room::sheet())) {
            view(style = || StyleApplication::new(Main::sheet())) { rows }
            side
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_rooms_are_as_square_as_they_can_be() {
        assert_eq!(split(1), (1, 1));
        assert_eq!(split(2), (2, 2));
        assert_eq!(split(4), (4, 2));
        assert_eq!(split(5), (5, 3));
        assert_eq!(split(9), (9, 3));
    }

    #[test]
    fn past_the_grid_the_rest_go_to_the_side() {
        let (grid, columns) = split(14);
        assert_eq!((grid, columns), (GRID_MAX, 3));
        assert_eq!(14 - grid, 5, "five in the side column");
    }

    #[test]
    fn an_empty_room_has_no_grid() {
        assert_eq!(split(0).0, 0);
    }

    #[test]
    fn a_focused_tile_takes_the_main_area_and_the_rest_go_aside() {
        let keys = ["screen:1", "me", "person:1", "person:3"];
        assert_eq!(arrange(&keys, Some("person:1")), (vec![2], vec![0, 1, 3]));
    }

    #[test]
    fn without_focus_it_is_the_grid() {
        let keys = ["me", "person:1"];
        assert_eq!(arrange(&keys, None), (vec![0, 1], vec![]));
    }

    #[test]
    fn a_focus_on_someone_who_left_falls_back_to_the_grid() {
        let keys = ["me", "person:1"];
        assert_eq!(arrange(&keys, Some("person:9")), (vec![0, 1], vec![]));
    }
}
