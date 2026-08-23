//! Hearth's data model and the app-wide state handle.
//!
//! The seed content is `static` — the same fixture `Hearth.dc.html` carries,
//! so the implementation can be compared against the design side by side.
//! Everything the *user* produces (sent messages, thread replies, mutes,
//! moderation, removals) lives in signals on [`App`].
//!
//! The split matters beyond the fixture: `SEED` is the shape a server
//! response has to arrive in, and every view reads rooms and messages through
//! the accessors below rather than touching the tables. Swapping the seed for
//! a live sync feed is a change to this module and nowhere else.

use runtime_core::{signal, Signal};

// =============================================================================
// Static shape — what a server would send
// =============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoomKind {
    /// A text channel.
    Text,
    /// A voice room. Always open; people come and go.
    Voice,
    /// A voice room where cameras are expected (but never required).
    Video,
    /// A voice room built around one person's screen.
    Share,
}

impl RoomKind {
    /// Text channels and DMs render the message column; the three live kinds
    /// render the room. This predicate is the whole routing rule.
    pub fn is_chat(self) -> bool {
        matches!(self, RoomKind::Text)
    }
}

pub struct Space {
    pub id: &'static str,
    pub name: &'static str,
    pub initials: &'static str,
    pub meta: &'static str,
}

pub struct Room {
    pub id: &'static str,
    pub space: &'static str,
    pub kind: RoomKind,
    pub name: &'static str,
    pub unread: bool,
    /// Who is in the room right now. Empty for text channels.
    pub people: &'static [&'static str],
}

pub struct Dm {
    pub id: &'static str,
    pub name: &'static str,
    pub status: Status,
    pub unread: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Online,
    Away,
    Offline,
}

pub struct Person {
    pub name: &'static str,
    pub status: Status,
    /// A short "what they're up to" line, shown under the name.
    pub note: &'static str,
}

pub struct Bio {
    pub name: &'static str,
    pub since: &'static str,
    pub where_: &'static str,
}

pub struct Seed {
    pub author: &'static str,
    pub time: &'static str,
    pub text: &'static str,
    pub thread: &'static [Reply],
}

pub struct Reply {
    pub author: &'static str,
    pub time: &'static str,
    pub text: &'static str,
}

/// One room's seeded transcript.
pub struct Transcript {
    pub room: &'static str,
    pub msgs: &'static [Seed],
}

// =============================================================================
// The fixture
// =============================================================================

pub static SPACES: &[Space] = &[
    Space { id: "cabin", name: "The Cabin", initials: "TC", meta: "12 people · 3 in rooms" },
    Space { id: "studio", name: "Studio", initials: "ST", meta: "8 people · quiet right now" },
    Space {
        id: "family",
        name: "Mercier Family",
        initials: "MF",
        meta: "6 people · Sunday call at 7",
    },
];

pub static ROOMS: &[Room] = &[
    // The Cabin
    Room { id: "welcome", space: "cabin", kind: RoomKind::Text, name: "welcome", unread: false, people: &[] },
    Room { id: "general", space: "cabin", kind: RoomKind::Text, name: "general", unread: true, people: &[] },
    Room { id: "trail-photos", space: "cabin", kind: RoomKind::Text, name: "trail-photos", unread: false, people: &[] },
    Room { id: "recipes", space: "cabin", kind: RoomKind::Text, name: "recipes", unread: false, people: &[] },
    Room { id: "porch", space: "cabin", kind: RoomKind::Voice, name: "The Porch", unread: false, people: &["Ana Ruiz", "Theo Lang", "Jun Park"] },
    Room { id: "movie", space: "cabin", kind: RoomKind::Video, name: "Movie Night", unread: false, people: &["Priya Nair", "Sam Oyelaran"] },
    Room { id: "desk", space: "cabin", kind: RoomKind::Share, name: "Desk Share", unread: false, people: &["Ana Ruiz", "Marcus Bell"] },
    // Studio
    Room { id: "standup", space: "studio", kind: RoomKind::Text, name: "standup", unread: true, people: &[] },
    Room { id: "design", space: "studio", kind: RoomKind::Text, name: "design", unread: false, people: &[] },
    Room { id: "ship-log", space: "studio", kind: RoomKind::Text, name: "ship-log", unread: false, people: &[] },
    Room { id: "warroom", space: "studio", kind: RoomKind::Voice, name: "War Room", unread: false, people: &["Nadia Fox"] },
    Room { id: "review", space: "studio", kind: RoomKind::Share, name: "Design Review", unread: false, people: &[] },
    // Mercier Family
    Room { id: "kitchen", space: "family", kind: RoomKind::Text, name: "kitchen-table", unread: false, people: &[] },
    Room { id: "photos", space: "family", kind: RoomKind::Text, name: "photos", unread: false, people: &[] },
    Room { id: "sunday", space: "family", kind: RoomKind::Video, name: "Sunday Call", unread: false, people: &["Mom", "Dad"] },
];

pub static DMS: &[Dm] = &[
    Dm { id: "dm-ana", name: "Ana Ruiz", status: Status::Online, unread: false },
    Dm { id: "dm-theo", name: "Theo Lang", status: Status::Away, unread: true },
    Dm { id: "dm-mom", name: "Mom", status: Status::Offline, unread: false },
];

pub static PEOPLE: &[Person] = &[
    Person { name: "Ana Ruiz", status: Status::Online, note: "On The Porch" },
    Person { name: "Jun Park", status: Status::Online, note: "On The Porch" },
    Person { name: "Priya Nair", status: Status::Online, note: "" },
    Person { name: "Theo Lang", status: Status::Away, note: "Back in an hour" },
    Person { name: "Sam Oyelaran", status: Status::Online, note: "" },
    Person { name: "Marcus Bell", status: Status::Offline, note: "" },
    Person { name: "Nadia Fox", status: Status::Offline, note: "" },
    Person { name: "Mom", status: Status::Offline, note: "" },
];

pub static BIOS: &[Bio] = &[
    Bio { name: "Ana Ruiz", since: "March 2024", where_: "Portland · on The Porch" },
    Bio { name: "Theo Lang", since: "March 2024", where_: "Back in an hour" },
    Bio { name: "Jun Park", since: "April 2024", where_: "On The Porch" },
    Bio { name: "Priya Nair", since: "June 2024", where_: "Around" },
    Bio { name: "Sam Oyelaran", since: "July 2024", where_: "Around" },
    Bio { name: "Marcus Bell", since: "March 2024", where_: "Last seen yesterday" },
    Bio { name: "Nadia Fox", since: "January 2024", where_: "Last seen Friday" },
    Bio { name: "Mom", since: "The beginning", where_: "Offline" },
    Bio { name: "Dad", since: "The beginning", where_: "Offline" },
];

pub static TRANSCRIPTS: &[Transcript] = &[
    Transcript {
        room: "general",
        msgs: &[
            Seed { author: "Ana Ruiz", time: "9:02", text: "Morning. The trail gate is finally fixed — whoever wrestled with it last weekend, you're free.", thread: &[] },
            Seed { author: "Theo Lang", time: "9:04", text: "That was me. I lost. Gate won.", thread: &[] },
            Seed { author: "Theo Lang", time: "9:04", text: "Anyway — anyone up for the ridge loop Saturday? Weather looks kind.", thread: &[] },
            Seed {
                author: "Jun Park",
                time: "9:11",
                text: "I'm in. Starting early though, I want to be back before the heat.",
                thread: &[
                    Reply { author: "Ana Ruiz", time: "9:13", text: "7am at the lower lot?" },
                    Reply { author: "Jun Park", time: "9:14", text: "Perfect. I can drive four." },
                    Reply { author: "Priya Nair", time: "9:20", text: "Save me a seat — I'll bring the thermos." },
                ],
            },
            Seed { author: "Priya Nair", time: "9:26", text: "I'll be on The Porch this afternoon if anyone wants to hang out while they work. No agenda.", thread: &[] },
            Seed { author: "You", time: "9:31", text: "Might drop in around two.", thread: &[] },
            Seed { author: "Sam Oyelaran", time: "10:02", text: "Posted the ridge photos from last month in trail-photos, in case anyone needs convincing.", thread: &[] },
        ],
    },
    Transcript {
        room: "welcome",
        msgs: &[
            Seed { author: "Ana Ruiz", time: "Mon", text: "This is the whole thing: a few rooms to talk in, a few rooms to sit in. Say hello whenever.", thread: &[] },
            Seed { author: "Marcus Bell", time: "Mon", text: "Took me about four seconds to figure out. Refreshing.", thread: &[] },
        ],
    },
    Transcript {
        room: "trail-photos",
        msgs: &[
            Seed { author: "Sam Oyelaran", time: "8:40", text: "Ridge, last light. Worth the early start.", thread: &[] },
            Seed { author: "Ana Ruiz", time: "8:52", text: "That is unreasonably pretty.", thread: &[] },
        ],
    },
    Transcript {
        room: "recipes",
        msgs: &[
            Seed { author: "Priya Nair", time: "Tue", text: "Thermos chai, since three people asked: strong black tea, whole spices, half milk, don't rush it.", thread: &[] },
            Seed { author: "Theo Lang", time: "Tue", text: "Adding \"do not rush it\" to my whole life.", thread: &[] },
        ],
    },
    Transcript {
        room: "standup",
        msgs: &[
            Seed { author: "Nadia Fox", time: "9:00", text: "Yesterday: finished the invite flow. Today: settings. Blocked on nothing.", thread: &[] },
            Seed { author: "You", time: "9:03", text: "Reviewing the invite flow this morning.", thread: &[] },
            Seed {
                author: "Marcus Bell",
                time: "9:07",
                text: "Shipping the room list cleanup after lunch.",
                thread: &[
                    Reply { author: "Nadia Fox", time: "9:09", text: "Does that include the collapsed groups?" },
                    Reply { author: "Marcus Bell", time: "9:12", text: "Yep, both." },
                ],
            },
        ],
    },
    Transcript {
        room: "design",
        msgs: &[Seed { author: "Nadia Fox", time: "11:14", text: "Cut the third nav level. Nobody used it and it made everything feel like software.", thread: &[] }],
    },
    Transcript {
        room: "ship-log",
        msgs: &[Seed { author: "Marcus Bell", time: "Fri", text: "v0.9 — rooms stay open, calls stay quiet, no notification firehose.", thread: &[] }],
    },
    Transcript {
        room: "kitchen",
        msgs: &[
            Seed { author: "Mom", time: "7:15", text: "Sunday call at seven, as always. Bring nothing but yourselves.", thread: &[] },
            Seed { author: "Dad", time: "7:22", text: "I'll be in the video room early, fiddling with the camera as tradition demands.", thread: &[] },
            Seed { author: "You", time: "8:01", text: "See you both then.", thread: &[] },
        ],
    },
    Transcript {
        room: "photos",
        msgs: &[Seed { author: "Mom", time: "Sun", text: "Found a box of the lake pictures. Scanning a few each week.", thread: &[] }],
    },
    Transcript {
        room: "dm-ana",
        msgs: &[
            Seed { author: "Ana Ruiz", time: "9:40", text: "Are you around for ten minutes? Easier to say out loud.", thread: &[] },
            Seed { author: "You", time: "9:41", text: "Yes — hop on The Porch, I'm already there.", thread: &[] },
            Seed { author: "Ana Ruiz", time: "9:41", text: "On my way.", thread: &[] },
        ],
    },
    Transcript {
        room: "dm-theo",
        msgs: &[
            Seed { author: "Theo Lang", time: "Yest", text: "Did the gate hold?", thread: &[] },
            Seed { author: "You", time: "Yest", text: "It held.", thread: &[] },
        ],
    },
    Transcript {
        room: "dm-mom",
        msgs: &[Seed { author: "Mom", time: "Sat", text: "Call whenever. No rush.", thread: &[] }],
    },
];

// =============================================================================
// Lookups
// =============================================================================

pub fn space(id: &str) -> &'static Space {
    SPACES.iter().find(|s| s.id == id).unwrap_or(&SPACES[0])
}

pub fn rooms_in(space_id: &str) -> impl Iterator<Item = &'static Room> {
    let space_id = space_id.to_string();
    ROOMS.iter().filter(move |r| space_id == r.space)
}

pub fn room(id: &str) -> Option<&'static Room> {
    ROOMS.iter().find(|r| r.id == id)
}

pub fn dm(id: &str) -> Option<&'static Dm> {
    DMS.iter().find(|d| d.id == id)
}

pub fn dm_for(name: &str) -> Option<&'static Dm> {
    DMS.iter().find(|d| d.name == name)
}

pub fn person(name: &str) -> Option<&'static Person> {
    PEOPLE.iter().find(|p| p.name == name)
}

pub fn bio(name: &str) -> Option<&'static Bio> {
    BIOS.iter().find(|b| b.name == name)
}

pub fn transcript(room_id: &str) -> &'static [Seed] {
    TRANSCRIPTS.iter().find(|t| t.room == room_id).map(|t| t.msgs).unwrap_or(&[])
}

/// Someone's presence. Anyone not on the roster (`Dad`, who only ever appears
/// inside a room) reads as offline rather than panicking.
pub fn status_of(name: &str) -> Status {
    person(name).map(|p| p.status).unwrap_or(Status::Offline)
}

/// Two letters for the avatar: initials for a full name, first two letters
/// otherwise ("Mom" → "MO"). Uppercased, because a lowercase avatar reads as
/// a typo at 24px.
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

/// A stable colour per person, hashed from the name.
///
/// The point is that a face keeps its colour everywhere it appears — the
/// roster, a message, a room tile, the profile card — without anyone storing
/// an avatar. The multiplier and modulus are the design's, kept so a given
/// name lands on the same tone here as it does in `Hearth.dc.html`.
pub fn tone_index(name: &str) -> usize {
    let mut h: u32 = 0;
    for b in name.bytes() {
        h = (h.wrapping_mul(31).wrapping_add(b as u32)) % 9973;
    }
    (h % 7) as usize
}

// =============================================================================
// Live state
// =============================================================================

/// Where the room list is pointed: at a space's rooms, or at direct messages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Space,
    Dms,
}

/// A message the user sent this session.
#[derive(Clone, PartialEq, Debug)]
pub struct Sent {
    pub room: String,
    pub text: String,
}

/// A thread reply the user sent this session, keyed by the root message's
/// `room:index` address.
#[derive(Clone, PartialEq, Debug)]
pub struct SentReply {
    pub key: String,
    pub text: String,
}

/// Which panel the main column is showing. `Settings` is a peer of the room
/// views rather than a modal, matching the design.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Chat,
    Live,
    Settings,
}

/// How much of the notification firehose to let through. Three values, one
/// choice, no per-channel matrix — the design is emphatic about this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notify {
    Everything,
    Mentions,
    Nothing,
}

impl Notify {
    pub fn label(self) -> &'static str {
        match self {
            Notify::Everything => "Everything",
            Notify::Mentions => "Mentions",
            Notify::Nothing => "Nothing",
        }
    }
}

/// Which half of the join dialog is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinTab {
    Join,
    New,
}

/// Every piece of mutable app state, as one `Copy` handle.
///
/// `Signal` is `Copy`, so this struct is too — which is what lets a view take
/// `app: App` by value and move it into as many event closures as it needs
/// without a single `clone()`. The alternative (threading a dozen individual
/// signals through every function) is what this replaces.
#[derive(Clone, Copy)]
pub struct App {
    // --- navigation ---
    pub scope: Signal<Scope>,
    pub space_id: Signal<&'static str>,
    pub room_id: Signal<&'static str>,
    /// The last text channel or DM visited, so the mobile preview and the
    /// spaces rail have something to return to from a live room.
    pub last_chat: Signal<&'static str>,
    pub last_dm: Signal<&'static str>,
    pub settings_open: Signal<bool>,

    // --- composing ---
    pub draft: Signal<String>,
    pub composer_focused: Signal<bool>,
    pub sent: Signal<Vec<Sent>>,

    // --- threads ---
    /// `room:index` of the thread's root message, or empty for "closed".
    pub thread_key: Signal<String>,
    pub thread_draft: Signal<String>,
    pub sent_replies: Signal<Vec<SentReply>>,

    // --- overlays ---
    pub search_open: Signal<bool>,
    pub query: Signal<String>,
    pub join_open: Signal<bool>,
    pub join_tab: Signal<JoinTab>,
    pub join_value: Signal<String>,
    pub mobile_open: Signal<bool>,
    /// Name of the person whose profile card is open, or empty.
    pub profile: Signal<String>,
    /// Which person menu is open, as `site:name` — a person appears in
    /// several lists at once (roster, room faces, a DM row) and each site
    /// anchors its own menu, so the name alone would open all of them.
    /// Empty means closed.
    pub menu: Signal<String>,
    /// Name pending removal confirmation, or empty.
    pub remove_target: Signal<String>,

    // --- voice ---
    pub mic: Signal<bool>,
    pub deafened: Signal<bool>,
    pub cam: Signal<bool>,
    pub sharing: Signal<bool>,
    /// The room whose call you're actually in, or empty. Distinct from
    /// `room_id`: you keep hearing The Porch while reading #recipes.
    pub in_voice: Signal<&'static str>,

    // --- preferences ---
    pub dark: Signal<bool>,
    pub accent: Signal<crate::theme::Accent>,
    pub notify: Signal<Notify>,
    pub quiet_hours: Signal<bool>,
    pub compact: Signal<bool>,
    pub read_receipts: Signal<bool>,
    pub show_people: Signal<bool>,

    // --- moderation, all local to this session ---
    pub you_are_moderator: Signal<bool>,
    /// People you've muted for yourself.
    pub muted: Signal<Vec<String>>,
    /// People a moderator has muted in rooms.
    pub timed_out: Signal<Vec<String>>,
    pub moderators: Signal<Vec<String>>,
    pub removed: Signal<Vec<String>>,
}

impl App {
    pub fn new() -> Self {
        App {
            scope: signal(Scope::Space),
            space_id: signal("cabin"),
            room_id: signal("general"),
            last_chat: signal("general"),
            last_dm: signal("dm-ana"),
            settings_open: signal(false),

            draft: signal(String::new()),
            composer_focused: signal(false),
            sent: signal(Vec::new()),

            thread_key: signal(String::new()),
            thread_draft: signal(String::new()),
            sent_replies: signal(Vec::new()),

            search_open: signal(false),
            query: signal(String::new()),
            join_open: signal(false),
            join_tab: signal(JoinTab::Join),
            join_value: signal(String::new()),
            mobile_open: signal(false),
            profile: signal(String::new()),
            menu: signal(String::new()),
            remove_target: signal(String::new()),

            mic: signal(true),
            deafened: signal(false),
            cam: signal(false),
            sharing: signal(false),
            in_voice: signal(""),

            dark: signal(true),
            accent: signal(crate::theme::Accent::Indigo),
            notify: signal(Notify::Mentions),
            quiet_hours: signal(true),
            compact: signal(false),
            read_receipts: signal(true),
            show_people: signal(true),

            you_are_moderator: signal(true),
            muted: signal(Vec::new()),
            timed_out: signal(Vec::new()),
            moderators: signal(vec!["Ana Ruiz".to_string()]),
            removed: signal(Vec::new()),
        }
    }

    // --- derived reads -----------------------------------------------------

    /// The room currently selected, whether it's a channel or a DM. `None`
    /// only while a space has no rooms at all.
    pub fn current_room(&self) -> Option<&'static Room> {
        room(self.room_id.get())
    }

    /// The kind of thing selected, treating a DM as a text channel.
    pub fn current_kind(&self) -> RoomKind {
        self.current_room().map(|r| r.kind).unwrap_or(RoomKind::Text)
    }

    /// Which panel the main column should show.
    pub fn view(&self) -> View {
        if self.settings_open.get() {
            View::Settings
        } else if self.current_kind().is_chat() || self.viewing_dm() {
            View::Chat
        } else {
            View::Live
        }
    }

    /// True when the selected id names a direct-message conversation.
    pub fn viewing_dm(&self) -> bool {
        dm(self.room_id.get()).is_some()
    }

    /// The title for whatever is selected — `# general`, `The Porch`, or a
    /// person's name.
    pub fn room_title(&self) -> String {
        if let Some(d) = dm(self.room_id.get()) {
            return d.name.to_string();
        }
        match self.current_room() {
            Some(r) if r.kind.is_chat() => format!("# {}", r.name),
            Some(r) => r.name.to_string(),
            None => String::new(),
        }
    }

    /// The full transcript for a room: seed messages, then anything sent this
    /// session. `You` posts land at the end because that is where they'd land
    /// on a real server too.
    pub fn messages(&self, room_id: &str) -> Vec<Line> {
        let mut out: Vec<Line> = transcript(room_id)
            .iter()
            .enumerate()
            .map(|(i, m)| Line {
                key: format!("{room_id}:{i}"),
                author: m.author.to_string(),
                time: m.time.to_string(),
                text: m.text.to_string(),
                seeded_replies: m.thread.len(),
            })
            .collect();

        let base = out.len();
        out.extend(self.sent.with(|sent| {
            sent.iter()
                .filter(|s| s.room == room_id)
                .enumerate()
                .map(|(i, s)| Line {
                    key: format!("{room_id}:{}", base + i),
                    author: "You".to_string(),
                    time: "now".to_string(),
                    text: s.text.clone(),
                    seeded_replies: 0,
                })
                .collect::<Vec<_>>()
        }));
        out
    }

    /// How many replies a message's thread holds, seeded plus sent.
    pub fn reply_count(&self, key: &str, seeded: usize) -> usize {
        seeded + self.sent_replies.with(|r| r.iter().filter(|x| x.key == key).count())
    }

    /// The open thread's messages: the root, then its replies. Empty when no
    /// thread is open or the key no longer resolves.
    ///
    /// The root is looked up through [`messages`](Self::messages) rather than
    /// straight into the seed table, so a thread opened on a message *you*
    /// just sent shows that message at the top instead of an empty panel.
    pub fn thread_lines(&self) -> Vec<Line> {
        let key = self.thread_key.get();
        let Some((room_id, _)) = key.rsplit_once(':') else {
            return Vec::new();
        };
        let Some(root) = self.messages(room_id).into_iter().find(|l| l.key == key) else {
            return Vec::new();
        };
        // Seeded replies hang off the seed entry; a sent root has none.
        let seeded: &'static [Reply] = key
            .rsplit_once(':')
            .and_then(|(_, i)| i.parse::<usize>().ok())
            .and_then(|i| transcript(room_id).get(i))
            .map(|s| s.thread)
            .unwrap_or(&[]);

        let mut out = vec![Line { key: format!("{key}:root"), ..root }];
        out.extend(seeded.iter().enumerate().map(|(i, r)| Line {
            key: format!("{key}:{i}"),
            author: r.author.to_string(),
            time: r.time.to_string(),
            text: r.text.to_string(),
            seeded_replies: 0,
        }));
        let base = out.len();
        out.extend(self.sent_replies.with(|replies| {
            replies
                .iter()
                .filter(|r| r.key == key)
                .enumerate()
                .map(|(i, r)| Line {
                    key: format!("{key}:{}", base + i),
                    author: "You".to_string(),
                    time: "now".to_string(),
                    text: r.text.clone(),
                    seeded_replies: 0,
                })
                .collect::<Vec<_>>()
        }));
        out
    }

    /// Everyone in a live room right now, including you when you've joined it.
    pub fn occupants(&self, r: &'static Room) -> Vec<String> {
        let mut out: Vec<String> = r.people.iter().map(|p| p.to_string()).collect();
        if self.in_voice.get() == r.id {
            out.push("You".to_string());
        }
        out
    }

    /// Messages matching the search box, across this space's rooms and every
    /// DM. Capped at 20 — a search that returns everything has answered
    /// nothing.
    pub fn search(&self) -> Vec<Hit> {
        let q = self.query.get().trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let space_id = self.space_id.get();
        let mut out = Vec::new();

        for r in rooms_in(space_id) {
            collect_hits(&q, r.id, &format!("# {}", r.name), &mut out);
        }
        for d in DMS {
            collect_hits(&q, d.id, d.name, &mut out);
        }
        out.truncate(20);
        out
    }

    /// Is this person muted for you?
    pub fn is_muted(&self, name: &str) -> bool {
        self.muted.with(|m| m.iter().any(|n| n == name))
    }

    pub fn is_timed_out(&self, name: &str) -> bool {
        self.timed_out.with(|m| m.iter().any(|n| n == name))
    }

    pub fn is_moderator(&self, name: &str) -> bool {
        self.moderators.with(|m| m.iter().any(|n| n == name))
    }

    pub fn is_removed(&self, name: &str) -> bool {
        self.removed.with(|m| m.iter().any(|n| n == name))
    }

    // --- commands ----------------------------------------------------------

    /// Select a room or DM. Closes settings, any open thread, and the search
    /// overlay — navigating away is an answer to all three — and clears the
    /// draft, because a half-typed line belongs to the room you left.
    pub fn go(&self, id: &'static str) {
        let is_dm = dm(id).is_some();
        self.room_id.set(id);
        self.scope.set(if is_dm { Scope::Dms } else { Scope::Space });
        if is_dm {
            self.last_dm.set(id);
        }
        if is_dm || room(id).map(|r| r.kind.is_chat()).unwrap_or(false) {
            self.last_chat.set(id);
        }
        self.settings_open.set(false);
        self.thread_key.set(String::new());
        self.search_open.set(false);
        self.draft.set(String::new());
    }

    /// Switch spaces, landing on the space's first room.
    pub fn go_space(&self, id: &'static str) {
        self.space_id.set(id);
        self.scope.set(Scope::Space);
        self.settings_open.set(false);
        self.thread_key.set(String::new());
        if let Some(first) = rooms_in(id).next() {
            self.room_id.set(first.id);
            self.last_chat.set(first.id);
        }
    }

    /// Open direct messages, returning to the last conversation.
    pub fn go_dms(&self) {
        self.scope.set(Scope::Dms);
        self.room_id.set(self.last_dm.get());
        self.settings_open.set(false);
        self.thread_key.set(String::new());
    }

    /// Post the draft to the current room. A blank draft is a no-op rather
    /// than an empty message.
    pub fn send(&self) {
        let text = self.draft.get().trim().to_string();
        if text.is_empty() {
            return;
        }
        let room = self.room_id.get().to_string();
        self.sent.update(|prev| {
            let mut next = prev.clone();
            next.push(Sent { room: room.clone(), text: text.clone() });
            next
        });
        self.draft.set(String::new());
    }

    /// Post the thread draft into the open thread.
    pub fn send_reply(&self) {
        let text = self.thread_draft.get().trim().to_string();
        let key = self.thread_key.get();
        if text.is_empty() || key.is_empty() {
            return;
        }
        self.sent_replies.update(|prev| {
            let mut next = prev.clone();
            next.push(SentReply { key: key.clone(), text: text.clone() });
            next
        });
        self.thread_draft.set(String::new());
    }

    /// Join the selected live room, or leave it if you're already in.
    pub fn toggle_voice(&self) {
        let Some(r) = self.current_room() else { return };
        if self.in_voice.get() == r.id {
            self.in_voice.set("");
            self.sharing.set(false);
        } else {
            self.in_voice.set(r.id);
        }
    }

    /// Leave whatever room you're in, from anywhere.
    pub fn leave_voice(&self) {
        self.in_voice.set("");
        self.sharing.set(false);
    }

    /// Deafening implies muting: you cannot be in a conversation you can't
    /// hear. Un-deafening does not un-mute — that's a separate decision.
    pub fn toggle_deafened(&self) {
        let now = !self.deafened.get();
        self.deafened.set(now);
        if now {
            self.mic.set(false);
        }
    }

    /// Open someone's profile card. `You` has no card — the design routes
    /// self to Settings instead, and a card about yourself is a dead end.
    pub fn open_profile(&self, name: &str) {
        if name.is_empty() || name == "You" {
            return;
        }
        self.profile.set(name.to_string());
        self.menu.set(String::new());
    }

    /// Open the person menu at one site. `You` has no menu — every item on it
    /// is something you'd do *to* someone else.
    pub fn open_menu(&self, site: &str, name: &str) {
        if name.is_empty() || name == "You" {
            return;
        }
        self.menu.set(format!("{site}:{name}"));
        self.profile.set(String::new());
    }

    /// Is the menu open at this site, and for whom?
    pub fn menu_at(&self, site: &str) -> Option<String> {
        let open = self.menu.get();
        open.strip_prefix(site)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(|name| name.to_string())
    }

    pub fn close_menu(&self) {
        self.menu.set(String::new());
    }

    /// Toggle a name's membership in one of the moderation lists.
    pub fn toggle_in(&self, list: Signal<Vec<String>>, name: &str) {
        let name = name.to_string();
        list.update(|prev| {
            let mut next = prev.clone();
            match next.iter().position(|n| *n == name) {
                Some(i) => {
                    next.remove(i);
                }
                None => next.push(name.clone()),
            }
            next
        });
    }

    /// Confirmed removal. Their messages stay; only their presence goes.
    pub fn confirm_remove(&self) {
        let name = self.remove_target.get();
        if !name.is_empty() {
            self.toggle_in(self.removed, &name);
        }
        self.remove_target.set(String::new());
        self.profile.set(String::new());
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// One rendered message, flattened out of seed + sent so views never have to
/// know which half it came from.
#[derive(Clone, PartialEq, Debug)]
pub struct Line {
    /// `room:index` — stable, so it keys both the list and the thread it opens.
    pub key: String,
    pub author: String,
    pub time: String,
    pub text: String,
    /// Replies present in the seed. Sent replies are counted separately so a
    /// `Line` stays a pure value.
    pub seeded_replies: usize,
}

/// One search result.
#[derive(Clone, PartialEq, Debug)]
pub struct Hit {
    pub key: String,
    pub room: &'static str,
    pub where_: String,
    pub author: String,
    pub text: String,
}

fn collect_hits(q: &str, room_id: &'static str, where_: &str, out: &mut Vec<Hit>) {
    for (i, m) in transcript(room_id).iter().enumerate() {
        if m.text.to_lowercase().contains(q) || m.author.to_lowercase().contains(q) {
            out.push(Hit {
                key: format!("{room_id}:{i}"),
                room: room_id,
                where_: where_.to_string(),
                author: m.author.to_string(),
                text: m.text.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_handle_one_and_two_word_names() {
        assert_eq!(initials("Ana Ruiz"), "AR");
        assert_eq!(initials("Mom"), "MO");
        assert_eq!(initials("Sam Oyelaran"), "SO");
    }

    /// A face keeps its colour everywhere it appears — that is the entire
    /// point of hashing rather than storing a tone per person.
    #[test]
    fn tone_index_is_stable_and_in_range() {
        for p in PEOPLE {
            let a = tone_index(p.name);
            assert_eq!(a, tone_index(p.name));
            assert!(a < 7, "{} hashed out of the tone table", p.name);
        }
    }

    /// Every room a space lists must belong to it, and every seeded
    /// transcript must name a room or DM that exists — a typo in either
    /// table renders as an empty channel with no error.
    #[test]
    fn fixture_tables_are_internally_consistent() {
        for s in SPACES {
            assert!(rooms_in(s.id).next().is_some(), "space {} has no rooms", s.id);
        }
        for t in TRANSCRIPTS {
            assert!(
                room(t.room).is_some() || dm(t.room).is_some(),
                "transcript for unknown room {}",
                t.room
            );
        }
        for r in ROOMS {
            assert!(SPACES.iter().any(|s| s.id == r.space), "room {} has no space", r.id);
            if r.kind.is_chat() {
                assert!(r.people.is_empty(), "text channel {} lists occupants", r.id);
            }
        }
    }

    /// Every name that appears as an occupant or an author must have a bio,
    /// or their profile card renders blank.
    #[test]
    fn everyone_who_speaks_or_sits_in_a_room_has_a_bio() {
        for r in ROOMS {
            for p in r.people {
                assert!(bio(p).is_some(), "{p} sits in {} with no bio", r.id);
            }
        }
        for t in TRANSCRIPTS {
            for m in t.msgs {
                if m.author != "You" {
                    assert!(bio(m.author).is_some(), "{} speaks with no bio", m.author);
                }
            }
        }
    }
}

// =============================================================================
// Render keys
// =============================================================================
//
// idealyst is fine-grained reactive: a `#[component]` body runs ONCE, and
// updates reach the screen through reactive sinks — a `text(closure)`, a style
// closure, a `when`, or a `switch`. A bare `signal.get()` in a component body
// is a one-time read, so a region built that way renders its initial state and
// then never changes, no matter how many handlers fire.
//
// Hearth's regions are built from state rather than bound field-by-field, so
// each one wraps itself in a `switch` keyed on the state it reads. The key
// types below ARE those dependency declarations: reading a signal to build one
// is what subscribes the region to it.
//
// Over-declaring a dependency costs one extra rebuild of one region.
// Under-declaring it costs a region that silently stops updating — so these
// lean deliberately wide.

/// Where you are: which space, which room, and which panel is showing.
#[derive(Clone, PartialEq)]
pub struct NavKey {
    scope: Scope,
    space_id: &'static str,
    room_id: &'static str,
    settings: bool,
    thread: String,
    in_voice: &'static str,
}

/// The local audio/video state, which the tray, the room, and the call bar all
/// paint from.
#[derive(Clone, PartialEq)]
pub struct VoiceKey {
    mic: bool,
    deafened: bool,
    cam: bool,
    sharing: bool,
    in_voice: &'static str,
}

/// Everything moderation can change about how a person is shown.
#[derive(Clone, PartialEq)]
pub struct PeopleKey {
    muted: Vec<String>,
    timed_out: Vec<String>,
    moderators: Vec<String>,
    removed: Vec<String>,
    menu: String,
    profile: String,
}

/// How much has been said. Counts rather than contents: a message is only ever
/// appended, so the length changing is exactly when the transcript changed.
#[derive(Clone, PartialEq)]
pub struct ContentKey {
    sent: usize,
    replies: usize,
}

/// The settings that change how things look.
#[derive(Clone, PartialEq)]
pub struct PrefsKey {
    dark: bool,
    accent: crate::theme::Accent,
    notify: Notify,
    quiet: bool,
    compact: bool,
    receipts: bool,
    show_people: bool,
    mobile: bool,
}

impl App {
    pub fn nav_key(&self) -> NavKey {
        NavKey {
            scope: self.scope.get(),
            space_id: self.space_id.get(),
            room_id: self.room_id.get(),
            settings: self.settings_open.get(),
            thread: self.thread_key.get(),
            in_voice: self.in_voice.get(),
        }
    }

    pub fn voice_key(&self) -> VoiceKey {
        VoiceKey {
            mic: self.mic.get(),
            deafened: self.deafened.get(),
            cam: self.cam.get(),
            sharing: self.sharing.get(),
            in_voice: self.in_voice.get(),
        }
    }

    pub fn people_key(&self) -> PeopleKey {
        PeopleKey {
            muted: self.muted.get(),
            timed_out: self.timed_out.get(),
            moderators: self.moderators.get(),
            removed: self.removed.get(),
            menu: self.menu.get(),
            profile: self.profile.get(),
        }
    }

    pub fn content_key(&self) -> ContentKey {
        ContentKey { sent: self.sent.with(|s| s.len()), replies: self.sent_replies.with(|s| s.len()) }
    }

    pub fn prefs_key(&self) -> PrefsKey {
        PrefsKey {
            dark: self.dark.get(),
            accent: self.accent.get(),
            notify: self.notify.get(),
            quiet: self.quiet_hours.get(),
            compact: self.compact.get(),
            receipts: self.read_receipts.get(),
            show_people: self.show_people.get(),
            mobile: self.mobile_open.get(),
        }
    }
}
