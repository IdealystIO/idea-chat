//! Client state.
//!
//! Everything the UI renders comes from the server now. This module holds the
//! signals it lands in, the selection state around them, and the small amount
//! of derivation the views need — grouping consecutive messages, deciding what
//! counts as unread.
//!
//! It used to hold a `static` fixture of the design's content. That fixture
//! keyed people by display name, used `&'static str` throughout, and gave
//! messages positional identity, none of which survives contact with a server.
//! The types here are the wire types (`crate::api::types`) plus selection —
//! deliberately, so there is one shape to keep in step rather than two.

use runtime_core::{signal, Signal};

use crate::api::types::{ChannelSummary, DmDto, Me, MemberDto, MessageDto, SpaceSummary};

/// What the whole app is doing right now.
///
/// A five-state enum rather than a pile of booleans: "connecting", "signed
/// out" and "needs claiming" are mutually exclusive, and every combination of
/// booleans that could express them is a bug waiting to render two screens at
/// once.
#[derive(Clone, PartialEq, Debug)]
pub enum Phase {
    /// Asking the server what it is. Nothing is renderable yet.
    Connecting,
    /// A fresh instance with no accounts. The operator holds a claim link.
    NeedsClaim,
    /// No session. Sign in, or redeem an invite.
    SignedOut,
    /// Signed in, with the shell's data loaded.
    Ready,
    /// The server could not be reached at all — distinct from "signed out",
    /// because the answer is "try again", not "sign in".
    Unreachable(String),
}

/// Which auth screen is showing while [`Phase::SignedOut`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthTab {
    SignIn,
    Invite,
}

/// Where the room list is pointed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Space,
    Dms,
}

/// Which panel the main column shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Chat,
    Live,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notify {
    Everything,
    Mentions,
    Nothing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinTab {
    Join,
    New,
}

/// A channel's kind, parsed from the wire's string.
///
/// The wire carries a string because it is a compatibility surface; the UI
/// wants an enum because it branches on it. Anything unrecognised reads as
/// `Text`, so a server that grows a new kind renders as a channel rather than
/// breaking a client that predates it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelKind {
    Text,
    Voice,
    Dm,
}

impl ChannelKind {
    /// `video` and `share` fold into `Voice` rather than being their own
    /// kinds. A voice room *is* the video room: camera and screen share are
    /// things you turn on once you are in one, not separate places to be. The
    /// old values are still accepted because databases written before this
    /// change still hold them — they now render as the voice rooms they always
    /// effectively were.
    pub fn parse(s: &str) -> ChannelKind {
        match s {
            "voice" | "video" | "share" => ChannelKind::Voice,
            "dm" => ChannelKind::Dm,
            _ => ChannelKind::Text,
        }
    }

    /// Text channels and DMs render the message column; voice rooms are not a
    /// place the main column ever goes — see [`App::view`].
    pub fn is_chat(self) -> bool {
        matches!(self, ChannelKind::Text | ChannelKind::Dm)
    }
}

/// Someone's presence.
///
/// The server does not report this yet — every member reads as `Online`. It
/// exists so the roster's shape is right for when it does, rather than having
/// to grow a field through every view later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Online,
    Away,
    Offline,
}

/// One rendered message row.
///
/// Flattened out of [`MessageDto`] with the presentation decisions applied:
/// whether it heads a group, what its timestamp reads as, whether it is a
/// tombstone.
#[derive(Clone, PartialEq, Debug)]
pub struct Line {
    pub id: String,
    pub author_id: Option<String>,
    pub author: String,
    pub time: String,
    pub text: String,
    pub reply_count: i32,
    /// First of a run by this author — gets the avatar and the name.
    pub heads: bool,
    pub deleted: bool,
    pub edited: bool,
}

/// Two letters for an avatar.
pub fn initials(name: &str) -> String {
    let mut parts = name.split_whitespace();
    let first = parts.next().unwrap_or("");
    match parts.next() {
        Some(second) => format!(
            "{}{}",
            first.chars().next().unwrap_or(' '),
            second.chars().next().unwrap_or(' ')
        ),
        None => first.chars().take(2).collect(),
    }
    .to_uppercase()
}

/// A stable colour per person.
///
/// Hashed from the **id**, not the display name — the fixture hashed the name,
/// which meant renaming yourself recoloured every message you had ever sent.
pub fn tone_index(id: &str) -> usize {
    let mut h: u32 = 0;
    for b in id.bytes() {
        h = (h.wrapping_mul(31).wrapping_add(b as u32)) % 9973;
    }
    (h % 6) as usize
}

/// Render an RFC 3339 timestamp as a short clock time.
///
/// Deliberately crude: it slices the wire format rather than converting to the
/// reader's timezone, because doing that properly needs a timezone database
/// the wasm bundle does not carry. It shows UTC, and it is the obvious place
/// to improve.
pub fn short_time(rfc3339: &str) -> String {
    rfc3339.split('T').nth(1).and_then(|t| t.get(..5)).unwrap_or("").to_string()
}

/// Every piece of client state, as one `Copy` handle.
#[derive(Clone, Copy)]
pub struct App {
    // --- session ---
    pub phase: Signal<Phase>,
    pub me: Signal<Option<Me>>,
    /// The message under an auth form — an error, or what is happening.
    pub notice: Signal<String>,
    pub busy: Signal<bool>,
    pub auth_tab: Signal<AuthTab>,
    pub instance_name: Signal<String>,

    // --- auth form fields ---
    pub form_identifier: Signal<String>,
    pub form_password: Signal<String>,
    pub form_handle: Signal<String>,
    pub form_display_name: Signal<String>,
    pub form_invite: Signal<String>,
    pub form_claim_token: Signal<String>,

    // --- server data ---
    pub spaces: Signal<Vec<SpaceSummary>>,
    pub dms: Signal<Vec<DmDto>>,
    pub members: Signal<Vec<MemberDto>>,
    pub messages: Signal<Vec<MessageDto>>,
    pub thread_messages: Signal<Vec<MessageDto>>,
    pub unread: Signal<Vec<String>>,
    /// Bumped whenever loaded data changes, so a region can depend on
    /// "something arrived" without naming every signal that might carry it.
    pub revision: Signal<u64>,

    // --- selection ---
    pub scope: Signal<Scope>,
    pub space_id: Signal<String>,
    pub channel_id: Signal<String>,
    pub last_chat: Signal<String>,
    pub last_dm: Signal<String>,
    pub settings_open: Signal<bool>,
    pub thread_root: Signal<String>,

    // --- composing ---
    pub draft: Signal<String>,
    pub composer_focused: Signal<bool>,
    pub thread_draft: Signal<String>,

    // --- overlays ---
    pub search_open: Signal<bool>,
    pub query: Signal<String>,
    pub join_open: Signal<bool>,
    pub join_tab: Signal<JoinTab>,
    pub join_value: Signal<String>,
    /// The invite code the join dialog minted, shown to be copied.
    pub minted_invite: Signal<String>,
    /// Whether the navigation drawer is open. Only meaningful below the pin
    /// breakpoint — at desktop widths the rail and room list are pinned and
    /// this is ignored. See `client::Shell`.
    pub nav_open: Signal<bool>,
    /// Whether the roster OVERLAY is open. Distinct from `show_people`, which
    /// is whether the pinned roster column is shown on a wide screen: a column
    /// you keep visible and a panel you slide over the conversation are not
    /// the same preference, and sharing one signal makes the app open a drawer
    /// on a phone because of a choice made on a desktop.
    pub roster_open: Signal<bool>,
    pub profile: Signal<String>,
    pub menu: Signal<String>,
    pub remove_target: Signal<String>,

    // --- voice, still local-only ---
    pub mic: Signal<bool>,
    pub deafened: Signal<bool>,
    pub cam: Signal<bool>,
    pub sharing: Signal<bool>,
    /// The room you are connected to. Independent of what you are reading.
    ///
    /// Set the moment you tap a room — before the socket opens — because a
    /// room you have joined should look joined. Whether audio is actually
    /// flowing is `voice_live`, and the tray tells the two apart.
    pub in_voice: Signal<String>,
    /// Whether the call is actually up: microphone open, socket connected.
    pub voice_live: Signal<bool>,
    /// Why the last join failed, if it did.
    pub voice_error: Signal<String>,
    /// Whether the live connection is up and the state is being kept current.
    pub live_up: Signal<bool>,
    /// Who is in each of this space's voice rooms, as the server sees it.
    ///
    /// Not derived from `in_voice`: this is everyone, including people in
    /// rooms you are not in. Seeing that three people are already talking is
    /// most of the reason to join them.
    pub voice_rooms: Signal<Vec<crate::api::types::RoomPresence>>,
    /// The room whose conference view is open, or empty.
    ///
    /// Separate from [`in_voice`] because being *in* a call and *looking at*
    /// one are different things. Joining a room must not move you off the
    /// channel you were reading, so connection lives in `in_voice` and the
    /// main column only shows the call when you explicitly ask for it.
    pub stage: Signal<String>,

    // --- preferences ---
    pub dark: Signal<bool>,
    pub accent: Signal<crate::theme::Accent>,
    pub notify: Signal<Notify>,
    pub quiet_hours: Signal<bool>,
    pub compact: Signal<bool>,
    pub read_receipts: Signal<bool>,
    pub show_people: Signal<bool>,

    // --- moderation, still local-only ---
    pub muted: Signal<Vec<String>>,
}

impl App {
    pub fn new() -> Self {
        App {
            phase: signal(Phase::Connecting),
            me: signal(None),
            notice: signal(String::new()),
            busy: signal(false),
            auth_tab: signal(AuthTab::SignIn),
            instance_name: signal("Hearth".to_string()),

            form_identifier: signal(String::new()),
            form_password: signal(String::new()),
            form_handle: signal(String::new()),
            form_display_name: signal(String::new()),
            form_invite: signal(String::new()),
            form_claim_token: signal(String::new()),

            spaces: signal(Vec::new()),
            dms: signal(Vec::new()),
            members: signal(Vec::new()),
            messages: signal(Vec::new()),
            thread_messages: signal(Vec::new()),
            unread: signal(Vec::new()),
            revision: signal(0),

            scope: signal(Scope::Space),
            space_id: signal(String::new()),
            channel_id: signal(String::new()),
            last_chat: signal(String::new()),
            last_dm: signal(String::new()),
            settings_open: signal(false),
            thread_root: signal(String::new()),

            draft: signal(String::new()),
            composer_focused: signal(false),
            thread_draft: signal(String::new()),

            search_open: signal(false),
            query: signal(String::new()),
            join_open: signal(false),
            join_tab: signal(JoinTab::Join),
            join_value: signal(String::new()),
            minted_invite: signal(String::new()),
            nav_open: signal(false),
            roster_open: signal(false),
            profile: signal(String::new()),
            menu: signal(String::new()),
            remove_target: signal(String::new()),

            mic: signal(true),
            deafened: signal(false),
            cam: signal(false),
            sharing: signal(false),
            in_voice: signal(String::new()),
            voice_live: signal(false),
            voice_error: signal(String::new()),
            live_up: signal(false),
            voice_rooms: signal(Vec::new()),
            stage: signal(String::new()),

            dark: signal(true),
            accent: signal(crate::theme::Accent::Indigo),
            notify: signal(Notify::Mentions),
            quiet_hours: signal(true),
            compact: signal(false),
            read_receipts: signal(true),
            show_people: signal(true),

            muted: signal(Vec::new()),
        }
    }

    // --- derived reads ---------------------------------------------------

    pub fn space(&self) -> Option<SpaceSummary> {
        let id = self.space_id.get();
        self.spaces.with(|s| s.iter().find(|s| s.id == id).cloned())
    }

    /// The selected channel, whether it lives in a space or is a conversation.
    pub fn channel(&self) -> Option<ChannelSummary> {
        let id = self.channel_id.get();
        let in_space = self.spaces.with(|spaces| {
            spaces.iter().flat_map(|s| s.channels.iter()).find(|c| c.id == id).cloned()
        });
        in_space.or_else(|| {
            self.dms.with(|dms| {
                dms.iter().find(|d| d.channel_id == id).map(|d| ChannelSummary {
                    id: d.channel_id.clone(),
                    kind: "dm".into(),
                    name: d.other_name.clone(),
                    topic: None,
                })
            })
        })
    }

    pub fn channel_kind(&self) -> ChannelKind {
        self.channel().map(|c| ChannelKind::parse(&c.kind)).unwrap_or(ChannelKind::Text)
    }

    pub fn viewing_dm(&self) -> bool {
        self.channel_kind() == ChannelKind::Dm
    }

    /// What the main column shows.
    ///
    /// A voice room is never selected as the channel — clicking one connects
    /// you and leaves you where you were reading. The call view appears only
    /// when [`stage`](Self::stage) is set, which takes an explicit second act.
    pub fn view(&self) -> View {
        if self.settings_open.get() {
            View::Settings
        } else if !self.stage.get().is_empty() {
            View::Live
        } else {
            View::Chat
        }
    }

    /// The header title: `# general`, `The Porch`, or a person's name.
    pub fn channel_title(&self) -> String {
        match self.channel() {
            Some(c) if ChannelKind::parse(&c.kind) == ChannelKind::Text => format!("# {}", c.name),
            Some(c) => c.name,
            None => String::new(),
        }
    }

    /// The channels of the current space, by kind.
    pub fn channels_of_kind(&self, chat: bool) -> Vec<ChannelSummary> {
        self.space()
            .map(|s| {
                s.channels
                    .into_iter()
                    .filter(|c| ChannelKind::parse(&c.kind).is_chat() == chat)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn is_unread(&self, channel_id: &str) -> bool {
        // Never mark the channel you are looking at: you are reading it.
        self.channel_id.get() != channel_id
            && self.unread.with(|u| u.iter().any(|c| c == channel_id))
    }

    pub fn my_id(&self) -> String {
        self.me.with(|m| m.as_ref().map(|m| m.id.clone()).unwrap_or_default())
    }

    pub fn my_name(&self) -> String {
        self.me
            .with(|m| m.as_ref().map(|m| m.display_name.clone()))
            .unwrap_or_else(|| "You".into())
    }

    /// My role in the current space.
    pub fn my_role(&self) -> String {
        self.space().map(|s| s.role).unwrap_or_else(|| "member".into())
    }

    pub fn moderates(&self) -> bool {
        matches!(self.my_role().as_str(), "owner" | "moderator")
    }

    pub fn is_muted(&self, user_id: &str) -> bool {
        self.muted.with(|m| m.iter().any(|u| u == user_id))
    }

    pub fn member(&self, user_id: &str) -> Option<MemberDto> {
        self.members.with(|m| m.iter().find(|m| m.id == user_id).cloned())
    }

    /// The current channel's messages as rendered rows.
    ///
    /// Grouping happens here rather than in the view because it depends on a
    /// message's *neighbour*, which a per-row component cannot see.
    pub fn lines(&self) -> Vec<Line> {
        self.messages.with(|msgs| to_lines(msgs))
    }

    /// The open thread's messages. Every reply carries its author — a thread
    /// is short enough that grouping saves nothing and costs attribution.
    pub fn thread_lines(&self) -> Vec<Line> {
        self.thread_messages.with(|msgs| msgs.iter().map(|m| one_line(m, true)).collect())
    }

    /// Messages in the open channel matching the search box.
    ///
    /// Client-side over what is already loaded, which is honest about its
    /// limits: it finds things in the current channel's history, not across
    /// the instance. A real search is a server endpoint and a Postgres index.
    pub fn search_hits(&self) -> Vec<Line> {
        let q = self.query.get().trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        self.lines()
            .into_iter()
            .filter(|l| {
                !l.deleted
                    && (l.text.to_lowercase().contains(&q) || l.author.to_lowercase().contains(&q))
            })
            .take(20)
            .collect()
    }

    // --- commands --------------------------------------------------------

    pub fn toggle_in(&self, list: Signal<Vec<String>>, value: &str) {
        let value = value.to_string();
        list.update(|prev| {
            let mut next = prev.clone();
            match next.iter().position(|v| *v == value) {
                Some(i) => {
                    next.remove(i);
                }
                None => next.push(value.clone()),
            }
            next
        });
    }

    pub fn open_profile(&self, user_id: &str) {
        if user_id.is_empty() || user_id == self.my_id() {
            return;
        }
        self.profile.set(user_id.to_string());
        self.menu.set(String::new());
    }

    pub fn open_menu(&self, site: &str, user_id: &str) {
        if user_id.is_empty() || user_id == self.my_id() {
            return;
        }
        self.menu.set(format!("{site}:{user_id}"));
        self.profile.set(String::new());
    }

    pub fn menu_at(&self, site: &str) -> Option<String> {
        let open = self.menu.get();
        open.strip_prefix(site).and_then(|r| r.strip_prefix(':')).map(|s| s.to_string())
    }

    pub fn close_menu(&self) {
        self.menu.set(String::new());
    }

    /// Connect to a room. Does not move you off what you are reading.
    ///
    /// Clicking a voice room joins it immediately — there is no separate
    /// "connect" step and no confirmation. Switching rooms while connected is
    /// just another join.
    ///
    /// On web this opens a real microphone and a real socket (see
    /// `crate::voice`). Everywhere else it moves the UI and says so, because
    /// playback on native is a framework gap and a call you cannot hear is
    /// worse than one the app admits it cannot make.
    pub fn join_voice(&self, id: &str) {
        #[cfg(all(not(feature = "server"), target_arch = "wasm32"))]
        crate::voice::join_room(*self, id);

        #[cfg(not(all(not(feature = "server"), target_arch = "wasm32")))]
        {
            self.in_voice.set(id.to_string());
            self.voice_live.set(false);
            self.voice_error.set("Voice needs the web build for now.".to_string());
        }
    }

    pub fn leave_voice(&self) {
        #[cfg(all(not(feature = "server"), target_arch = "wasm32"))]
        crate::voice::leave_room(*self);

        self.in_voice.set(String::new());
        self.voice_live.set(false);
        self.sharing.set(false);
        self.cam.set(false);
        // Nothing to look at once you are out of the call.
        self.stage.set(String::new());
    }

    /// Open the call view in the main column.
    pub fn open_stage(&self, id: &str) {
        self.stage.set(id.to_string());
        self.settings_open.set(false);
    }

    /// Put the main column back on the channel you were reading.
    pub fn close_stage(&self) {
        self.stage.set(String::new());
    }

    /// A fingerprint of who is where, for a `switch` key.
    ///
    /// Counts alone would not do: two people swapping rooms leaves every count
    /// unchanged, and the sidebar would keep drawing the wrong names. This
    /// includes identity, so any movement is a different key.
    pub fn presence_key(&self) -> String {
        self.voice_rooms.with(|rooms| {
            let mut key = String::new();
            for room in rooms {
                key.push_str(&room.channel_id);
                for p in &room.people {
                    key.push(',');
                    key.push_str(&p.id);
                }
                key.push(';');
            }
            key
        })
    }

    /// Who is in `channel`, for the row under a voice room.
    pub fn people_in_room(&self, channel: &str) -> Vec<crate::api::types::MemberDto> {
        self.voice_rooms.with(|rooms| {
            rooms
                .iter()
                .find(|r| r.channel_id == channel)
                .map(|r| r.people.clone())
                .unwrap_or_default()
        })
    }

    /// The name of the room you are connected to, for the tray and the header.
    pub fn voice_room_name(&self) -> String {
        let id = self.in_voice.get();
        if id.is_empty() {
            return String::new();
        }
        self.spaces
            .with(|spaces| {
                spaces
                    .iter()
                    .flat_map(|s| s.channels.iter())
                    .find(|c| c.id == id)
                    .map(|c| c.name.clone())
            })
            .unwrap_or_default()
    }

    /// Deafening implies muting: you cannot be in a conversation you cannot
    /// hear. Un-deafening does not un-mute — that is a separate decision.
    pub fn toggle_deafened(&self) {
        let now = !self.deafened.get();
        self.deafened.set(now);
        if now {
            self.mic.set(false);
        }
    }

    /// Note that something loaded, so regions keyed on `revision` rebuild.
    pub fn touch(&self) {
        self.revision.update(|r| r.wrapping_add(1));
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Group consecutive messages by the same author into runs.
fn to_lines(msgs: &[MessageDto]) -> Vec<Line> {
    msgs.iter()
        .enumerate()
        .map(|(i, m)| {
            let heads = i == 0 || msgs[i - 1].author_id != m.author_id;
            one_line(m, heads)
        })
        .collect()
}

fn one_line(m: &MessageDto, heads: bool) -> Line {
    Line {
        id: m.id.clone(),
        author_id: m.author_id.clone(),
        author: m.author_name.clone(),
        time: short_time(&m.created_at),
        // A tombstone still occupies a row: replies keep a parent, and a gap
        // in a conversation is more confusing than a note saying what
        // happened.
        text: m.body.clone().unwrap_or_else(|| "This message was deleted.".to_string()),
        reply_count: m.reply_count,
        heads,
        deleted: m.body.is_none(),
        edited: m.edited_at.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, author: &str) -> MessageDto {
        MessageDto {
            id: id.into(),
            channel_id: "c".into(),
            author_name: format!("User {author}"),
            author_id: Some(author.into()),
            thread_root_id: None,
            body: Some("hello".into()),
            reply_count: 0,
            created_at: "2026-08-24T09:02:13Z".into(),
            edited_at: None,
        }
    }

    #[test]
    fn initials_handle_one_and_two_word_names() {
        assert_eq!(initials("Ana Ruiz"), "AR");
        assert_eq!(initials("Mom"), "MO");
    }

    /// Hashing the id rather than the name is the point: renaming yourself
    /// must not recolour every message you have ever sent.
    #[test]
    fn a_persons_colour_follows_their_id_not_their_name() {
        let a = tone_index("018f0000-0000-7000-8000-000000000001");
        assert_eq!(a, tone_index("018f0000-0000-7000-8000-000000000001"));
        assert!(a < 6);
    }

    #[test]
    fn consecutive_messages_by_one_author_form_one_run() {
        let lines =
            to_lines(&[msg("1", "ana"), msg("2", "ana"), msg("3", "theo"), msg("4", "ana")]);
        assert!(lines[0].heads, "the first message always heads");
        assert!(!lines[1].heads, "a second from the same author joins the run");
        assert!(lines[2].heads, "a different author starts a new run");
        assert!(lines[3].heads, "and switching back starts another");
    }

    #[test]
    fn a_deleted_message_keeps_its_row_and_says_so() {
        let mut m = msg("1", "ana");
        m.body = None;
        let line = one_line(&m, true);
        assert!(line.deleted);
        assert_eq!(line.text, "This message was deleted.");
    }

    #[test]
    fn timestamps_render_as_a_short_clock_time() {
        assert_eq!(short_time("2026-08-24T09:02:13Z"), "09:02");
        assert_eq!(short_time("nonsense"), "");
    }

    /// An unrecognised kind must render as a channel, not break — a client
    /// that predates a new server kind still has to work.
    #[test]
    fn an_unknown_channel_kind_degrades_to_text() {
        assert_eq!(ChannelKind::parse("holodeck"), ChannelKind::Text);
        assert_eq!(ChannelKind::parse("voice"), ChannelKind::Voice);
        // Rooms written before video and share were folded into voice.
        assert_eq!(ChannelKind::parse("video"), ChannelKind::Voice);
        assert_eq!(ChannelKind::parse("share"), ChannelKind::Voice);
    }
}
