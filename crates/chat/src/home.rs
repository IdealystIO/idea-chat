//! Home, with no space chosen: your friends — who is around, requests
//! waiting, who you blocked — and, for someone new, a first step.

use chat_model::{Id, Presence, Relation};
use icons_lucide::{CHECK, MENU, MESSAGE_SQUARE, PHONE, USERS, USER_PLUS, X, BAN};
use idea_ui::{
    tone, typography_kind, variant, AvatarSize, Button, Field, Icon, IdeaThemeRef, SegmentOption, SegmentedControl,
    Typography,
};
use idea_ui_nav::sidebar_pinned;
use runtime_core::{
    component, memo, rx, signal, stylesheet, ui, AlignItems, Element, FlexDirection, FlexWrap, FontWeight,
    JustifyContent, Length, Signal,
};

use crate::bits::{presence_label, Action, Empty, PersonAvatar, RowLabel};
use crate::channel::{HeaderActions, HeaderTitle, StageHeader};
use crate::forms::{cb, on_enter, set, Form};
use crate::platform;
use crate::shell::{open_space, panel_inline, StageMain, StageRow, PIN_AT};
use crate::store::{store, Dialog};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Filter {
    #[default]
    Online,
    All,
    Pending,
    Blocked,
}

impl Filter {
    fn id(self) -> &'static str {
        match self {
            Filter::Online => "online",
            Filter::All => "all",
            Filter::Pending => "pending",
            Filter::Blocked => "blocked",
        }
    }

    fn from_id(id: &str) -> Filter {
        match id {
            "all" => Filter::All,
            "pending" => Filter::Pending,
            "blocked" => Filter::Blocked,
            _ => Filter::Online,
        }
    }
}

stylesheet! {
    List<()> {
        base(_t) {
            // Scroll content grows past the viewport rather than shrinking into it.
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            padding_left: 20.0,
            padding_right: 20.0,
            padding_top: 12.0,
            padding_bottom: 24.0,
        }
    }
}

stylesheet! {
    PersonRow<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 12.0,
            min_height: 60.0,
            border_bottom_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

stylesheet! {
    Count<()> {
        base(_t) {
            padding_top: 8.0,
            padding_bottom: 8.0,
        }
    }
}

stylesheet! {
    ActivePanel<IdeaThemeRef> {
        base(t) {
            width: 280.0,
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            gap: 12.0,
            padding_left: 16.0,
            padding_right: 16.0,
            padding_top: 20.0,
            background: t.color.background(),
            border_left_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

stylesheet! {
    ActiveCard<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Column,
            gap: 8.0,
            padding_left: 14.0,
            padding_right: 14.0,
            padding_top: 12.0,
            padding_bottom: 12.0,
            border_radius: 10.0,
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

stylesheet! {
    InlineRow<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 10.0,
        }
    }
}


#[component]
pub fn HomeStage() -> Element {
    let s = store();
    let ui = s.ui();
    let new_here = {
        let s = s.clone();
        memo(move || s.read(|st| st.spaces.is_empty() && st.relations.is_empty() && st.me.is_some()) && platform::pref("onboarded").is_none())
    };
    let filter: Signal<Filter> = signal(Filter::default());
    let wide = memo(move || panel_inline());
    ui! {
        view(style = StageRow()) {
            view(style = StageMain()) {
                view(style = StageHeader()) {
                    if !sidebar_pinned(PIN_AT) {
                        Action(icon = MENU, label = "Open navigation".to_string(), on_click = cb(move || ui.drawer.set(true)))
                    }
                    view(style = HeaderTitle()) {
                        Icon(data = USERS, size = 20.0)
                        Typography(content = "Friends".to_string(), weight = Some(FontWeight::SemiBold), kind = typography_kind::BodyLg)
                        if runtime_core::current_breakpoint().get().is_at_least(runtime_core::Breakpoint::Sm) {
                            SegmentedControl(
                                value = rx!(filter.get().id().to_string()),
                                on_change = crate::forms::cb1(move |id: String| filter.set(Filter::from_id(&id))),
                                options = vec![
                                    SegmentOption::new("online", "Online".to_string()),
                                    SegmentOption::new("all", "All".to_string()),
                                    SegmentOption::new("pending", "Pending".to_string()),
                                    SegmentOption::new("blocked", "Blocked".to_string()),
                                ],
                                size = idea_ui::ControlSize::Sm,
                            )
                        }
                    }
                    view(style = HeaderActions()) {
                        Button(label = "Add friend".to_string(), size = idea_ui::size::Sm, leading_icon = Some(USER_PLUS), on_click = cb(move || ui.dialog.set(Some(Dialog::AddFriend))))
                    }
                }
                if new_here.get() {
                    Onboarding()
                } else {
                    People(filter = filter)
                }
            }
            if wide.get() {
                ActiveNow()
            }
        }
    }
}

#[component]
fn People(#[prop(static, default = signal(Filter::default()))] filter: Signal<Filter>) -> Element {
    let s = store();
    let ui = s.ui();
    let list = memo(move || {
        let f = filter.get();
        s.read(|st| {
            let pick = |r: Relation| st.people(r).into_iter().map(|u| (u.id, r)).collect::<Vec<_>>();
            match f {
                Filter::Online => pick(Relation::Friend).into_iter().filter(|(u, _)| st.presence_of(*u) != Presence::Offline).collect(),
                Filter::All => pick(Relation::Friend),
                Filter::Pending => {
                    let mut v = pick(Relation::Incoming);
                    v.extend(pick(Relation::Outgoing));
                    v
                }
                Filter::Blocked => pick(Relation::Blocked),
            }
        })
    });
    let heading = memo(move || {
        let n = list.get().len();
        match filter.get() {
            Filter::Online => format!("ONLINE — {n}"),
            Filter::All => format!("ALL FRIENDS — {n}"),
            Filter::Pending => format!("PENDING — {n}"),
            Filter::Blocked => format!("BLOCKED — {n}"),
        }
    });
    ui! {
        if list.get().is_empty() {
            match filter.get() {
                Filter::Online => { Empty(icon = USERS, title = "Nobody's around right now".to_string(), body = "Friends who are online show up here.".to_string(), action = Some(("Add a friend".to_string(), cb(move || ui.dialog.set(Some(Dialog::AddFriend)))))) }
                Filter::All => { Empty(icon = USER_PLUS, title = "No friends yet".to_string(), body = "Add someone by their username, or meet people in a space.".to_string(), action = Some(("Add a friend".to_string(), cb(move || ui.dialog.set(Some(Dialog::AddFriend)))))) }
                Filter::Pending => { Empty(icon = USER_PLUS, title = "No requests".to_string(), body = "Friend requests you send and receive wait here.".to_string()) }
                Filter::Blocked => { Empty(icon = BAN, title = "Nobody blocked".to_string(), body = "People you block can't message you or see when you're online.".to_string()) }
            }
        } else {
            scroll_view(style = crate::bits::Fill()) {
                view(style = List()) {
                    view(style = Count()) { Typography(content = rx!(heading.get()), kind = typography_kind::Overline, muted = true) }
                    for (user, relation) in list, key = user.to_string() {
                        PersonLine(user = user, relation = relation)
                    }
                }
            }
        }
    }
}

#[component]
fn PersonLine(#[prop(static)] user: Id, #[prop(static, default = Relation::Friend)] relation: Relation) -> Element {
    let s = store();
    let line = memo(move || {
        s.read(|st| {
            let u = st.users.get(&user);
            (
                u.map(|u| u.display_name.clone()).unwrap_or_default(),
                u.map(|u| u.handle.clone()).unwrap_or_default(),
                st.status_text(user).map(str::to_string).unwrap_or_else(|| presence_label(st.presence_of(user)).to_string()),
            )
        })
    });
    let toast = |r: chat_api::Answer<()>| {
        if let Err(e) = r {
            idea_ui::push_toast(chat_api::describe(&e), tone::Danger);
        }
    };
    let accept = cb(move || runtime_core::spawn_then(chat_api::people::accept_friend(user), toast));
    let remove = cb(move || runtime_core::spawn_then(chat_api::people::remove_relation(user), toast));
    let message = cb(move || crate::overlays::message_person(user));
    let call = cb(move || crate::overlays::call_person(user));
    let sub = match relation {
        Relation::Incoming => "Wants to be friends".to_string(),
        Relation::Outgoing => "Request sent".to_string(),
        Relation::Blocked => "Blocked".to_string(),
        Relation::Friend => String::new(),
    };
    ui! {
        view(style = PersonRow()) {
            PersonAvatar(user = user, size = AvatarSize::Md, presence = relation == Relation::Friend)
            view(style = RowLabel()) {
                view(style = InlineRow()) {
                    Typography(content = rx!(line.get().0), weight = Some(FontWeight::SemiBold), lines = Some(1))
                    Typography(content = rx!(format!("@{}", line.get().1)), kind = typography_kind::Caption, muted = true, lines = Some(1))
                }
                Typography(content = rx!(if relation == Relation::Friend { line.get().2 } else { sub.clone() }), kind = typography_kind::Caption, muted = true, lines = Some(1))
            }
            match relation {
                Relation::Friend => {
                    Action(icon = MESSAGE_SQUARE, label = "Message".to_string(), variant = variant::Soft, on_click = message.clone())
                    Action(icon = PHONE, label = "Call".to_string(), variant = variant::Soft, on_click = call.clone())
                }
                Relation::Incoming => {
                    Action(icon = CHECK, label = "Accept".to_string(), tone = tone::Success, variant = variant::Soft, on_click = accept.clone())
                    Action(icon = X, label = "Decline".to_string(), tone = tone::Danger, variant = variant::Soft, on_click = remove.clone())
                }
                Relation::Outgoing => { Action(icon = X, label = "Cancel request".to_string(), variant = variant::Soft, on_click = remove.clone()) }
                Relation::Blocked => { Button(label = "Unblock".to_string(), variant = variant::Soft, size = idea_ui::size::Sm, on_click = remove.clone()) }
            }
        }
    }
}

/// Friends who are in a call right now, for the side of Home.
#[component]
fn ActiveNow() -> Element {
    let s = store();
    let active = memo(move || {
        s.read(|st| {
            let mut out: Vec<(Id, String)> = Vec::new();
            for (channel, people) in &st.voice {
                for user in people.keys() {
                    if st.relations.get(user) == Some(&Relation::Friend) {
                        let place = st.channels.get(channel).map(|c| match c.space.and_then(|sp| st.spaces.get(&sp)) {
                            Some(space) => format!("in {} · {}", c.name, space.name),
                            None => "in a call".to_string(),
                        });
                        out.push((*user, place.unwrap_or_default()));
                    }
                }
            }
            out
        })
    });
    ui! {
        view(style = ActivePanel()) {
            Typography(content = "Active now".to_string(), weight = Some(FontWeight::SemiBold), kind = typography_kind::BodyLg)
            if active.get().is_empty() {
                view(style = ActiveCard()) {
                    Typography(content = "It's quiet for now".to_string(), weight = Some(FontWeight::Medium))
                    Typography(content = "When a friend joins a call, they show up here.".to_string(), kind = typography_kind::BodySm, muted = true)
                }
            }
            for (user, place) in active, key = user.to_string() {
                view(style = ActiveCard()) {
                    view(style = InlineRow()) {
                        PersonAvatar(user = user, size = AvatarSize::Sm)
                        view(style = RowLabel()) {
                            Typography(content = store().peek(|st| st.display_name(user)), weight = Some(FontWeight::Medium), lines = Some(1))
                            Typography(content = place.clone(), kind = typography_kind::Caption, muted = true, lines = Some(1))
                        }
                    }
                }
            }
        }
    }
}

stylesheet! {
    Steps<()> {
        base(_t) {
            flex_grow: 1.0,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            gap: 24.0,
            padding_left: 16.0,
            padding_right: 16.0,
            padding_top: 24.0,
            padding_bottom: 24.0,
        }
    }
}

stylesheet! {
    Choices<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Wrap,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Stretch,
            gap: 20.0,
            max_width: Length::pct(100.0),
        }
    }
}

stylesheet! {
    Choice<IdeaThemeRef> {
        base(t) {
            width: 300.0,
            max_width: Length::pct(100.0),
            flex_direction: FlexDirection::Column,
            gap: 12.0,
            padding_left: 24.0,
            padding_right: 24.0,
            padding_top: 24.0,
            padding_bottom: 24.0,
            border_radius: 16.0,
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

/// Someone new, with no spaces and no friends: join one, or make one.
#[component]
fn Onboarding() -> Element {
    let ui = store().ui();
    let code = signal(crate::app::take_entry(|e| matches!(e, crate::app::Entry::Invite(_))).map(|e| match e {
        crate::app::Entry::Invite(c) => c,
        _ => String::new(),
    }).unwrap_or_default());
    let name = signal(String::new());
    let (join_form, make_form) = (Form::new(), Form::new());
    let join = cb(move || join_form.submit(chat_api::spaces::join_space(code.peek()), |space| {
        platform::set_pref("onboarded", "1");
        open_space(space);
    }));
    let make = cb(move || make_form.submit(chat_api::spaces::create_space(name.peek()), |space| {
        platform::set_pref("onboarded", "1");
        open_space(space);
    }));
    let skip = cb(move || {
        platform::set_pref("onboarded", "1");
        ui.dialog.set(Some(Dialog::AddFriend));
    });
    ui! {
        view(style = Steps()) {
            view(style = crate::bits::EmptyText()) {
                Typography(content = "Welcome to Idea Chat".to_string(), kind = typography_kind::H3, align = runtime_core::TextAlign::Center)
                Typography(content = "Spaces are where your groups talk. Join one you were invited to, or make your own.".to_string(), muted = true, align = runtime_core::TextAlign::Center)
            }
            view(style = Choices()) {
                view(style = Choice()) {
                    Typography(content = "Join with an invite".to_string(), kind = typography_kind::H6)
                    Typography(content = "Paste the link or code someone sent you.".to_string(), kind = typography_kind::BodySm, muted = true)
                    Field(value = code, on_change = set(code), placeholder = Some("Invite link or code".to_string()), error = rx!(join_form.error("code").or(join_form.general.get())), on_key_down = on_enter(join.clone()), accessibility_label = Some("Invite link or code".to_string()))
                    Button(label = "Join space".to_string(), on_click = join.clone(), loading = rx!(join_form.busy.get()))
                }
                view(style = Choice()) {
                    Typography(content = "Create a space".to_string(), kind = typography_kind::H6)
                    Typography(content = "For your friends, your team, your club.".to_string(), kind = typography_kind::BodySm, muted = true)
                    Field(value = name, on_change = set(name), placeholder = Some("Space name".to_string()), error = rx!(make_form.error("name").or(make_form.general.get())), on_key_down = on_enter(make.clone()), accessibility_label = Some("Space name".to_string()))
                    Button(label = "Create space".to_string(), variant = variant::Soft, on_click = make.clone(), loading = rx!(make_form.busy.get()))
                }
            }
            Button(label = "Skip for now — find friends instead".to_string(), variant = variant::Ghost, tone = tone::Neutral, size = idea_ui::size::Sm, on_click = skip)
        }
    }
}
