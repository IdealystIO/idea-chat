//! Calls: joining and leaving, the strip that follows you around while you
//! are in one, a voice channel's stage, and a ringing call.
//!
//! One call at a time. Joining asks the server for a ticket (relay, grant,
//! whether this device makes the room key); the media itself — the relay
//! link, the end-to-end key, the mic and camera — is `media`'s.

use chat_model::{ChannelKind, Id};
use icons_lucide::{HEADPHONES, HEADPHONE_OFF, MIC, MIC_OFF, PHONE, PHONE_OFF, VIDEO, VOLUME_2};
use idea_ui::{tone, typography_kind, variant, AvatarSize, Button, IdeaThemeRef, Modal, Typography};
use runtime_core::{
    component, memo, rx, signal, stylesheet, ui, AlignItems, Element, FlexDirection, FlexWrap, FontWeight,
    JustifyContent, Signal,
};

use crate::bits::{Action, PersonAvatar, RowLabel};
use crate::forms::cb;
use crate::shell::go;
use crate::store::{store, Place};

thread_local! {
    /// What this client asked for: mic and camera, before and during a call.
    static WANTS: std::cell::Cell<Option<Wants>> = const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy)]
struct Wants {
    muted: Signal<bool>,
    deafened: Signal<bool>,
    camera: Signal<bool>,
    screen: Signal<bool>,
}

fn wants() -> Wants {
    WANTS.with(|w| match w.get() {
        Some(w) => w,
        None => {
            let w = Wants { muted: signal(false), deafened: signal(false), camera: signal(false), screen: signal(false) };
            w.muted.set(false);
            WANTS.with(|c| c.set(Some(w)));
            w
        }
    })
}

/// Join a channel's call (leaving any other).
pub fn join(channel: Id) {
    // This is a click: the browser lets audio start now, so the cues can.
    crate::sounds::unlock();
    let Some((device, _)) = crate::device::current() else {
        idea_ui::push_toast("This device isn't registered yet — try again in a moment.", tone::Warning);
        return;
    };
    // Not `spawn_then`: that binds to the button pressed, and joining
    // replaces it — the server's VoiceJoined usually lands before this
    // reply, the channel view swaps "Join voice" for the room, and the
    // ticket would be dropped unread. A call outlives whatever started it.
    let alive = runtime_core::ScopeAlive::immortal();
    runtime_core::spawn_then_in(&alive, chat_api::calls::join_call(channel, device), move |r| match r {
        Ok(ticket) => {
            let w = wants();
            crate::media::start(ticket, w.muted.peek(), w.camera.peek());
            sync();
        }
        Err(e) => crate::forms::toast(&e),
    });
}

/// Back into a call we lost without leaving, on a fresh ticket: with the
/// mic and camera as they were.
pub fn resume(ticket: chat_model::CallTicket) {
    let w = wants();
    crate::media::start(ticket, w.muted.peek(), w.camera.peek());
    sync();
}

/// Join with the camera on.
pub fn join_with_camera(channel: Id) {
    wants().camera.set(true);
    join(channel);
}

pub fn leave() {
    crate::media::stop();
    let w = wants();
    w.camera.set(false);
    w.screen.set(false);
    runtime_core::spawn_then(chat_api::calls::leave_call(), |_| {});
}

/// Tell the server what we are sending, and the media layer what to send.
/// Given the values rather than reading them back: `Signal::set` stages,
/// so a read just after a set still sees the old value.
fn send_state(m: bool, d: bool, c: bool, sc: bool) {
    crate::media::set(m || d, d, c, sc);
    runtime_core::spawn_then(chat_api::calls::update_call(m || d, d, c, sc), |_| {});
}

fn sync() {
    let w = wants();
    send_state(w.muted.peek(), w.deafened.peek(), w.camera.peek(), w.screen.peek());
}

fn toggle(which: fn(&Wants) -> Signal<bool>) -> std::rc::Rc<dyn Fn()> {
    cb(move || {
        let w = wants();
        let s = which(&w);
        let now = !s.peek();
        s.set(now);
        let pick = |x: Signal<bool>| if x == s { now } else { x.peek() };
        send_state(pick(w.muted), pick(w.deafened), pick(w.camera), pick(w.screen));
    })
}

stylesheet! {
    Strip<IdeaThemeRef> {
        base(t) {
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            gap: 8.0,
            padding_left: 12.0,
            padding_right: 8.0,
            padding_top: 10.0,
            padding_bottom: 10.0,
            background: t.intent.primary.soft_bg(),
            border_top_width: 1.0,
            border_color: t.intent.primary.border(),
        }
    }
}

stylesheet! {
    StripTop<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 8.0,
        }
    }
}

stylesheet! {
    Live<IdeaThemeRef> {
        base(t) {
            width: 10.0,
            height: 10.0,
            border_radius: 999.0,
            background: t.intent.success.fg(),
            flex_shrink: 0.0,
        }
    }
}

stylesheet! {
    Controls<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 4.0,
        }
    }
}

/// Where you are, while you are in a call: at the foot of the navigation,
/// whatever you are looking at, with its controls.
#[component]
pub fn CallStrip() -> Element {
    let s = store();
    let call = {
        let s = s.clone();
        memo(move || {
            // Through a lost call's rejoin too, though the server no longer
            // lists us in it.
            let rejoining = crate::media::rejoining().get();
            s.read(|st| {
                let channel = st.my_call().map(|v| v.channel).or(rejoining)?;
                let c = st.channels.get(&channel)?;
                let place = match c.space.and_then(|sp| st.spaces.get(&sp)) {
                    Some(space) => format!("{} / {}", c.name, space.name),
                    None => st.title(c),
                };
                Some((channel, c.space, place))
            })
        })
    };
    let status = crate::media::status();
    let w = wants();
    ui! {
        match call.get() {
            Some((channel, space, place)) => {
                view(style = Strip()) {
                    Tap(on_press = { let (channel, space) = (*channel, *space); cb(move || match space { Some(space) => go(Place::Channel { space, channel }), None => go(Place::Direct(channel)) }) }, label = "Go to your call".to_string(), style = crate::nav::style(StripTop())) {
                        view(style = Live()) {}
                        view(style = RowLabel()) {
                            Typography(content = rx!(status.get().label().to_string()), kind = typography_kind::BodySm, weight = Some(FontWeight::SemiBold), tone = Some(tone::Success.into()))
                            Typography(content = place.clone(), kind = typography_kind::Caption, muted = true, lines = Some(1))
                        }
                    }
                    view(style = Controls()) {
                        Action(icon = rx!(if w.muted.get() { MIC_OFF } else { MIC }).get(), label = "Mute".to_string(), selected = rx!(w.muted.get()), on_click = toggle(|w| w.muted))
                        Action(icon = rx!(if w.deafened.get() { HEADPHONE_OFF } else { HEADPHONES }).get(), label = "Deafen".to_string(), selected = rx!(w.deafened.get()), on_click = toggle(|w| w.deafened))
                        Action(icon = VIDEO, label = "Camera".to_string(), selected = rx!(w.camera.get()), on_click = toggle(|w| w.camera))
                        view(style = RowLabel()) {}
                        Action(icon = PHONE_OFF, label = "Leave call".to_string(), tone = tone::Danger, variant = variant::Filled, on_click = cb(leave))
                    }
                }
            }
            None => { runtime_core::fragment(Vec::new()) }
        }
    }
}

use crate::media::Room;
use crate::nav::Tap;

stylesheet! {
    Stage<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            min_height: 0.0,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            gap: 20.0,
            padding_left: 16.0,
            padding_right: 16.0,
            background: t.color.background(),
        }
    }
}

stylesheet! {
    Faces<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            justify_content: JustifyContent::Center,
            gap: 8.0,
            max_width: 520.0,
        }
    }
}

/// A voice channel on the stage: who is in it and the way in — or, once
/// in, the call itself.
#[component]
pub fn VoiceStage(#[prop(static)] channel: Id) -> Element {
    let s = store();
    let me = s.me();
    let inside = {
        let s = s.clone();
        memo(move || s.read(|st| st.in_call(channel).iter().map(|v| v.user).collect::<Vec<Id>>()))
    };
    let joined = memo(move || me.is_some_and(|m| inside.get().contains(&m)));
    let name = s.peek(|st| st.channels.get(&channel).map(|c| c.name.clone()).unwrap_or_default());
    let who = memo(move || match inside.get().len() {
        0 => "Nobody's here yet".to_string(),
        1 => "1 person is here".to_string(),
        n => format!("{n} people are here"),
    });
    ui! {
        if joined.get() {
            Room(channel = channel)
        } else {
            view(style = Stage()) {
                view(style = Faces()) {
                    for user in inside, key = user.to_string() {
                        PersonAvatar(user = user, size = AvatarSize::Lg, presence = false, speaking = true)
                    }
                }
                view(style = crate::bits::EmptyText()) {
                    Typography(content = name.clone(), kind = typography_kind::H4, align = runtime_core::TextAlign::Center)
                    Typography(content = rx!(who.get()), muted = true, align = runtime_core::TextAlign::Center)
                }
                Button(label = "Join voice".to_string(), leading_icon = Some(VOLUME_2), tone = tone::Success, on_click = cb(move || join(channel)))
                Typography(content = "Calls are end-to-end encrypted.".to_string(), kind = typography_kind::Caption, muted = true)
            }
        }
    }
}

stylesheet! {
    RingCard<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            gap: 12.0,
            padding_top: 12.0,
            padding_bottom: 8.0,
        }
    }
}

stylesheet! {
    RingButtons<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            gap: 28.0,
            padding_top: 10.0,
        }
    }
}

/// An incoming call, over everything: who, and answer or decline.
#[component]
pub fn Ringing() -> Element {
    let s = store();
    let ring = {
        let s = s.clone();
        memo(move || s.read(|st| st.incoming_ring().map(|r| (r.channel, r.started_by, st.channels.get(&r.channel).map(|c| c.kind)))))
    };
    ui! {
        match ring.get() {
            Some((channel, from, kind)) => {
                Modal(open = true, width = 320.0, dismissable = false, content = {
                    let (channel, from, kind) = (*channel, *from, *kind);
                    move || {
                        let s = store();
                        let name = s.peek(|st| st.display_name(from));
                        let what = if kind == Some(ChannelKind::Group) { "is calling the group" } else { "is calling you" };
                        let answer = cb(move || {
                            go(Place::Direct(channel));
                            join(channel);
                        });
                        let decline = cb(move || runtime_core::spawn_then(chat_api::calls::decline_call(channel), |_| {}));
                        ui! {
                            view(style = RingCard()) {
                                PersonAvatar(user = from, size = AvatarSize::Xl, presence = false)
                                Typography(content = name.clone(), kind = typography_kind::H5)
                                Typography(content = what.to_string(), muted = true)
                                view(style = RingButtons()) {
                                    Action(icon = PHONE_OFF, label = "Decline".to_string(), tone = tone::Danger, variant = variant::Filled, size_px = Some(56.0), on_click = decline)
                                    Action(icon = PHONE, label = "Answer".to_string(), tone = tone::Success, variant = variant::Filled, size_px = Some(56.0), on_click = answer)
                                }
                            }
                        }
                    }
                })
            }
            None => { runtime_core::fragment(Vec::new()) }
        }
    }
}
