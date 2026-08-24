//! What Hearth keeps synchronized, and the one function that changes it.
//!
//! This module is compiled into **both** the server and the client. That is
//! the point: [`Live::apply`] is the single definition of what an event means,
//! so a state reached by reconnecting and a state reached by twenty live
//! events are the same state, computed by the same code.
//!
//! ```text
//!   click a voice room
//!        │
//!        ├─ Action::JoinVoice ───────────────▶ server
//!        │                                     can you see this channel?
//!        │                                     write presence
//!        │                                     decide: Event::VoiceJoined
//!        │ ◀──────────────────────────────────  to everyone in the space
//!        ▼
//!   apply(&mut state, &event)            ...and the same call runs on the
//!                                        server, folding it into the
//!                                        projection new clients snapshot.
//! ```
//!
//! Actions are requests and may be refused. Events are facts and never are.

use serde::{Deserialize, Serialize};

use crate::api::types::{MemberDto, MessageDto, RoomPresence};

/// The model. See [`idea_state::Model`].
pub struct Live;

/// Everything the app keeps in step with the server.
///
/// Deliberately small to begin with: voice presence was the thing being
/// polled, so it is the thing that moves first. Messages and read-state belong
/// here too and will arrive as more variants, not as another mechanism.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Who is in which voice room, across the spaces this connection can see.
    pub voice: Vec<RoomPresence>,
    /// Channels with something in them you have not read.
    pub unread: Vec<String>,
    /// The channel this connection is reading, if any.
    ///
    /// Part of the state rather than a client-side detail because it decides
    /// what `transcript` holds — and the server is the one that fills it.
    pub watching: Option<String>,
    /// The messages of the watched channel.
    ///
    /// One page, not all of history: a snapshot has to be a bounded thing, and
    /// scrollback is unbounded by definition. Paging further back is still a
    /// request; everything from the moment you started watching arrives as
    /// events.
    pub transcript: Vec<MessageDto>,
}

/// Something that happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum Event {
    /// Someone is now in a voice room.
    ///
    /// Carries the space so the event can be routed and folded without a
    /// lookup — a receiver should never have to ask a question to understand
    /// what it was told.
    VoiceJoined {
        /// The space the room belongs to.
        space_id: String,
        /// The room.
        channel_id: String,
        /// Who, described exactly as the roster describes them, so the sidebar
        /// draws one person one way everywhere.
        person: MemberDto,
    },
    /// Someone is no longer in a voice room.
    VoiceLeft {
        /// The space the room belongs to.
        space_id: String,
        /// The room.
        channel_id: String,
        /// Who left.
        user_id: String,
    },
    /// Somebody said something.
    ///
    /// Sent to everyone who can see the channel, whether or not they are
    /// reading it — the ones who are get it in their transcript, the ones who
    /// are not get an unread mark. One event, because it is one fact.
    MessagePosted {
        /// Where.
        channel_id: String,
        /// The message, exactly as a page of history would describe it.
        message: MessageDto,
    },
    /// A message was edited.
    MessageEdited {
        /// Where.
        channel_id: String,
        /// Which one.
        message_id: String,
        /// The new body.
        body: String,
    },
    /// A message was withdrawn.
    ///
    /// A tombstone, not a hole: the message keeps its place and says it was
    /// deleted, so a conversation does not silently rearrange itself around
    /// the gap.
    MessageDeleted {
        /// Where.
        channel_id: String,
        /// Which one.
        message_id: String,
    },
    /// This account has read a channel up to a point.
    ///
    /// Addressed to the account rather than the space: it is nobody else's
    /// business, and it exists so a second device stops showing a pip for
    /// something you read on the first.
    ReadUpTo {
        /// Where.
        channel_id: String,
    },
}

/// Something a client would like to happen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "a", rename_all = "snake_case")]
pub enum Action {
    /// Put me in this voice room.
    JoinVoice {
        /// The room.
        channel_id: String,
    },
    /// Take me out of this voice room.
    LeaveVoice {
        /// The room.
        channel_id: String,
    },
    /// I am still in the room I said I was in.
    ///
    /// Presence expires on the server, so a long call has to keep saying so.
    /// It is an action rather than a transport-level ping because it means
    /// something to the model — the transport's own keepalive says the socket
    /// is open, which is a different claim.
    StillHere {
        /// The room.
        channel_id: String,
    },
    /// I am reading this channel now.
    ///
    /// The server answers by restating the world with that channel's page in
    /// it. Switching channels is therefore one round trip and no request.
    Watch {
        /// The channel.
        channel_id: String,
    },
    /// Post this.
    ///
    /// The id is minted by the client so a retry is idempotent — the same
    /// contract the HTTP endpoint has, for the same reason.
    Post {
        /// The message's id, chosen here.
        id: String,
        /// Where.
        channel_id: String,
        /// What.
        body: String,
        /// The message this replies to, for a thread.
        thread_root_id: Option<String>,
    },
}

impl idea_state::Model for Live {
    type State = State;
    type Event = Event;
    type Action = Action;

    fn apply(state: &mut State, event: &Event) {
        match event {
            Event::VoiceJoined { space_id, channel_id, person } => {
                // One room at a time — the same invariant the database keys on.
                // Enforced here as well because the client folds events without
                // ever seeing the table, and "join" implies "left wherever you
                // were" without a separate event saying so.
                remove_person(state, &person.id);

                match state.voice.iter_mut().find(|r| &r.channel_id == channel_id) {
                    Some(room) => {
                        // Tolerating a repeat: a snapshot can overlap an event
                        // already folded in, and the trait requires that be a
                        // no-op rather than a duplicate face in the sidebar.
                        if !room.people.iter().any(|p| p.id == person.id) {
                            room.people.push(person.clone());
                        }
                    }
                    None => state.voice.push(RoomPresence {
                        channel_id: channel_id.clone(),
                        space_id: space_id.clone(),
                        people: vec![person.clone()],
                    }),
                }
            }
            Event::VoiceLeft { channel_id, user_id, .. } => {
                if let Some(room) = state.voice.iter_mut().find(|r| &r.channel_id == channel_id) {
                    room.people.retain(|p| &p.id != user_id);
                }
                // An empty room is absence, not a room with nobody in it — so
                // the two representations cannot disagree.
                state.voice.retain(|r| !r.people.is_empty());
            }

            Event::MessagePosted { channel_id, message } => {
                if state.watching.as_deref() == Some(channel_id.as_str()) {
                    // Reading it, so it belongs in the transcript — and reading
                    // it is what makes it read.
                    if !state.transcript.iter().any(|m| m.id == message.id) {
                        state.transcript.push(message.clone());
                        // Ids are UUIDv7, so lexical order IS arrival order.
                        // Sorting rather than trusting the push handles the one
                        // case that matters: a message that raced the page it
                        // belongs after.
                        state.transcript.sort_by(|a, b| a.id.cmp(&b.id));
                    }
                    state.unread.retain(|c| c != channel_id);
                } else if !state.unread.contains(channel_id) {
                    state.unread.push(channel_id.clone());
                }
            }

            Event::MessageEdited { channel_id, message_id, body } => {
                if state.watching.as_deref() == Some(channel_id.as_str()) {
                    if let Some(m) = state.transcript.iter_mut().find(|m| &m.id == message_id) {
                        m.body = Some(body.clone());
                    }
                }
            }

            Event::MessageDeleted { channel_id, message_id } => {
                if state.watching.as_deref() == Some(channel_id.as_str()) {
                    if let Some(m) = state.transcript.iter_mut().find(|m| &m.id == message_id) {
                        // A tombstone keeps its place. Removing the row would
                        // rearrange the conversation around the gap and orphan
                        // anything replying to it.
                        m.body = None;
                    }
                }
            }

            Event::ReadUpTo { channel_id } => {
                state.unread.retain(|c| c != channel_id);
            }
        }
    }
}

/// Take someone out of whatever room they are in.
fn remove_person(state: &mut State, user_id: &str) {
    for room in state.voice.iter_mut() {
        room.people.retain(|p| p.id != user_id);
    }
    state.voice.retain(|r| !r.people.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;
    use idea_state::Model;

    fn person(id: &str) -> MemberDto {
        MemberDto {
            id: id.to_string(),
            handle: id.to_string(),
            display_name: id.to_uppercase(),
            role: "member".to_string(),
        }
    }

    fn joined(space: &str, channel: &str, who: &str) -> Event {
        Event::VoiceJoined {
            space_id: space.into(),
            channel_id: channel.into(),
            person: person(who),
        }
    }

    fn people_in(state: &State, channel: &str) -> Vec<String> {
        state
            .voice
            .iter()
            .find(|r| r.channel_id == channel)
            .map(|r| r.people.iter().map(|p| p.id.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn joining_puts_someone_in_the_room() {
        let mut s = State::default();
        Live::apply(&mut s, &joined("sp", "porch", "ana"));
        assert_eq!(people_in(&s, "porch"), vec!["ana"]);
    }

    /// The trait requires folding the same event twice to be a no-op — a
    /// resnapshot can overlap events already applied.
    #[test]
    fn folding_the_same_join_twice_changes_nothing() {
        let mut s = State::default();
        Live::apply(&mut s, &joined("sp", "porch", "ana"));
        let once = s.clone();
        Live::apply(&mut s, &joined("sp", "porch", "ana"));
        assert_eq!(s, once);
    }

    /// A join is also a leave. There is no separate event for the room they
    /// were in, so the reducer has to know that.
    #[test]
    fn joining_a_second_room_leaves_the_first() {
        let mut s = State::default();
        Live::apply(&mut s, &joined("sp", "porch", "ana"));
        Live::apply(&mut s, &joined("sp", "movies", "ana"));
        assert!(people_in(&s, "porch").is_empty(), "left behind in the old room");
        assert_eq!(people_in(&s, "movies"), vec!["ana"]);
    }

    #[test]
    fn leaving_removes_only_that_person() {
        let mut s = State::default();
        Live::apply(&mut s, &joined("sp", "porch", "ana"));
        Live::apply(&mut s, &joined("sp", "porch", "theo"));
        Live::apply(
            &mut s,
            &Event::VoiceLeft {
                space_id: "sp".into(),
                channel_id: "porch".into(),
                user_id: "ana".into(),
            },
        );
        assert_eq!(people_in(&s, "porch"), vec!["theo"]);
    }

    /// An empty room disappears rather than lingering as an empty entry, so
    /// "nobody is here" has exactly one representation.
    #[test]
    fn a_room_that_empties_disappears() {
        let mut s = State::default();
        Live::apply(&mut s, &joined("sp", "porch", "ana"));
        Live::apply(
            &mut s,
            &Event::VoiceLeft {
                space_id: "sp".into(),
                channel_id: "porch".into(),
                user_id: "ana".into(),
            },
        );
        assert!(s.voice.is_empty(), "an empty room was left behind: {:?}", s.voice);
    }

    /// Events a client cannot make sense of must not corrupt anything.
    #[test]
    fn a_leave_for_someone_who_was_never_here_is_ignored() {
        let mut s = State::default();
        Live::apply(&mut s, &joined("sp", "porch", "ana"));
        let before = s.clone();
        Live::apply(
            &mut s,
            &Event::VoiceLeft {
                space_id: "sp".into(),
                channel_id: "elsewhere".into(),
                user_id: "nobody".into(),
            },
        );
        assert_eq!(s, before);
    }

    fn msg(id: &str, channel: &str, author: &str, body: &str) -> MessageDto {
        MessageDto {
            id: id.to_string(),
            channel_id: channel.to_string(),
            author_name: author.to_uppercase(),
            author_id: Some(author.to_string()),
            thread_root_id: None,
            body: Some(body.to_string()),
            reply_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            edited_at: None,
        }
    }

    fn watching(channel: &str) -> State {
        State { watching: Some(channel.to_string()), ..Default::default() }
    }

    #[test]
    fn a_message_in_the_channel_you_are_reading_joins_the_transcript() {
        let mut s = watching("general");
        Live::apply(
            &mut s,
            &Event::MessagePosted {
                channel_id: "general".into(),
                message: msg("01", "general", "ana", "hello"),
            },
        );
        assert_eq!(s.transcript.len(), 1);
        assert!(s.unread.is_empty(), "the channel you are reading is not unread");
    }

    /// The same event, two outcomes, decided by what this client is doing —
    /// which is exactly why it is one event and not two.
    #[test]
    fn a_message_in_another_channel_marks_it_unread_instead() {
        let mut s = watching("general");
        Live::apply(
            &mut s,
            &Event::MessagePosted {
                channel_id: "random".into(),
                message: msg("01", "random", "theo", "hello"),
            },
        );
        assert!(s.transcript.is_empty());
        assert_eq!(s.unread, vec!["random".to_string()]);
    }

    /// The optimistic row and the server's copy share an id, so the arriving
    /// event is a correction rather than a second line.
    #[test]
    fn a_message_already_in_the_transcript_is_not_added_twice() {
        let mut s = watching("general");
        let m = msg("01", "general", "ana", "hello");
        Live::apply(&mut s, &Event::MessagePosted { channel_id: "general".into(), message: m.clone() });
        Live::apply(&mut s, &Event::MessagePosted { channel_id: "general".into(), message: m });
        assert_eq!(s.transcript.len(), 1);
    }

    /// Ids are UUIDv7, so lexical order is arrival order — a message that
    /// races the page it belongs after still lands in the right place.
    #[test]
    fn the_transcript_stays_in_order() {
        let mut s = watching("general");
        for id in ["03", "01", "02"] {
            Live::apply(
                &mut s,
                &Event::MessagePosted {
                    channel_id: "general".into(),
                    message: msg(id, "general", "ana", id),
                },
            );
        }
        let ids: Vec<&str> = s.transcript.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["01", "02", "03"]);
    }

    #[test]
    fn a_deleted_message_keeps_its_place_as_a_tombstone() {
        let mut s = watching("general");
        Live::apply(
            &mut s,
            &Event::MessagePosted {
                channel_id: "general".into(),
                message: msg("01", "general", "ana", "hello"),
            },
        );
        Live::apply(
            &mut s,
            &Event::MessageDeleted { channel_id: "general".into(), message_id: "01".into() },
        );
        assert_eq!(s.transcript.len(), 1, "the row was removed instead of tombstoned");
        assert_eq!(s.transcript[0].body, None);
    }

    #[test]
    fn an_edit_changes_the_body_and_nothing_else() {
        let mut s = watching("general");
        Live::apply(
            &mut s,
            &Event::MessagePosted {
                channel_id: "general".into(),
                message: msg("01", "general", "ana", "frist"),
            },
        );
        Live::apply(
            &mut s,
            &Event::MessageEdited {
                channel_id: "general".into(),
                message_id: "01".into(),
                body: "first".into(),
            },
        );
        assert_eq!(s.transcript[0].body.as_deref(), Some("first"));
        assert_eq!(s.transcript[0].author_name, "ANA");
    }

    #[test]
    fn reading_a_channel_clears_its_mark() {
        let mut s = watching("general");
        Live::apply(
            &mut s,
            &Event::MessagePosted {
                channel_id: "random".into(),
                message: msg("01", "random", "theo", "hi"),
            },
        );
        Live::apply(&mut s, &Event::ReadUpTo { channel_id: "random".into() });
        assert!(s.unread.is_empty());
    }

    /// The property the whole design rests on: replaying a history and
    /// receiving a snapshot must land in the same place.
    #[test]
    fn replaying_events_reaches_the_same_state_whatever_the_starting_point() {
        let history = [
            joined("sp", "porch", "ana"),
            joined("sp", "porch", "theo"),
            joined("sp", "movies", "ana"),
            Event::VoiceLeft {
                space_id: "sp".into(),
                channel_id: "porch".into(),
                user_id: "theo".into(),
            },
            joined("sp", "movies", "wren"),
        ];

        let mut from_scratch = State::default();
        for e in &history {
            Live::apply(&mut from_scratch, e);
        }

        // A client that snapshotted midway and applied the rest live.
        let mut midway = State::default();
        for e in &history[..2] {
            Live::apply(&mut midway, e);
        }
        let snapshot = midway.clone();
        let mut resumed = snapshot;
        for e in &history[2..] {
            Live::apply(&mut resumed, e);
        }

        assert_eq!(from_scratch, resumed);
    }
}
