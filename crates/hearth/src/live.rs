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

use crate::api::types::{MemberDto, RoomPresence};

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
