//! A client/server reducer.
//!
//! The server owns the state. A connection is served a **snapshot** of it, and
//! from then on both sides move through the same sequence of **events**,
//! applied by the same pure function. A client never computes state from a
//! request/response; it applies what it is told.
//!
//! ```text
//!   client                                  server
//!     │  connect ───────────────────────────▶ │
//!     │ ◀──────────────── Down::Snapshot ──── │   the whole visible state
//!     │                                       │
//!     │  Up::Action(JoinVoice) ─────────────▶ │   an intent
//!     │                                       │   authorize, write, decide
//!     │ ◀──────────────── Down::Event ─────── │   a fact, to everyone who
//!     │  apply(&mut state, &event)            │   can see it
//! ```
//!
//! # Why a reducer and not "just push updates"
//!
//! Because [`Model::apply`] is one pure function compiled into both binaries,
//! there is exactly one definition of what an event means. The usual way this
//! goes wrong is two implementations — a server that writes a row and a client
//! that pokes a signal — which agree until the day they do not, and then
//! disagree silently until someone reloads. Here a reload and a live update
//! run identical code.
//!
//! It also makes the client's state a value rather than a pile of signals
//! mutated from callbacks: everything that can change it is an event, and
//! every event is named.
//!
//! # What this is not
//!
//! Not CRDTs and not offline-first — that is what the `sync` SDK is for, with
//! its outbox and merge policies. This assumes a live connection and a single
//! authority. Reconnecting takes a **fresh snapshot** rather than replaying a
//! log, which is why nothing here has to keep one.
//!
//! # Layering
//!
//! Nothing in this crate knows about HTTP, WebSockets, databases, or the UI
//! framework. It is the vocabulary: a [`Model`], the wire [`Up`]/[`Down`]
//! frames, an abstract [`Store`], and a [`Topic`] to address events with. The
//! app supplies the types and the transport.

#![deny(missing_docs)]

use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
pub mod hub;

/// One synchronized world: its state, the events that change it, and the
/// actions a client may ask for.
///
/// The three types are deliberately separate. An **action** is what a client
/// wants ("put me in this room") and may be refused. An **event** is what
/// happened ("this person is in this room") and is never refused — by the time
/// one exists, it is true. A client that conflated them would be predicting
/// the server's decisions.
pub trait Model: 'static {
    /// Everything a client renders from.
    type State: Clone + Send + Sync + 'static;
    /// A fact, already decided.
    type Event: Clone + Send + Sync + 'static;
    /// An intent, not yet decided.
    type Action: Clone + Send + Sync + 'static;

    /// Fold an event into the state.
    ///
    /// **Pure, total, and identical on both sides** — this is the whole point
    /// of the crate. No IO, no clock, no randomness: given the same state and
    /// the same event, every party must reach the same state, or the two sides
    /// drift and only a reload reveals it.
    ///
    /// It must also tolerate events it cannot make sense of — a duplicate join,
    /// a leave for a room that is already empty — because a client can be
    /// resnapshotted at any moment and may see an event it has already folded
    /// in. Ignoring is the correct handling; panicking would take the app down
    /// for a race that costs nothing.
    fn apply(state: &mut Self::State, event: &Self::Event);
}

/// Who an event is for.
///
/// Events are addressed, not broadcast. A topic is whatever scope the app can
/// authorize as a unit — `space:<id>` here — and a connection only receives
/// topics it was subscribed to when it was authorized. Getting this wrong is
/// how a live update becomes a data leak, so it is a required part of
/// publishing rather than an option on it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Topic(pub String);

impl Topic {
    /// A topic for one space and everyone in it.
    pub fn space(id: &str) -> Topic {
        Topic(format!("space:{id}"))
    }

    /// A topic for exactly one account, across their devices.
    pub fn user(id: &str) -> Topic {
        Topic(format!("user:{id}"))
    }

    /// A topic for one conversation and everyone who may read it.
    ///
    /// Finer than a space on purpose: a direct message has no space, and
    /// publishing one to a space topic would deliver it to everybody in it.
    pub fn channel(id: &str) -> Topic {
        Topic(format!("channel:{id}"))
    }
}

impl std::fmt::Display for Topic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Client to server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Up<A> {
    /// Please do this.
    Action(A),
    /// Still here. The server replies with [`Down::Pong`]; a connection that
    /// stops answering is gone whatever the socket believes.
    Ping,
}

/// Server to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Down<S, E> {
    /// The whole visible state. Sent on connect, and again whenever the server
    /// would rather restate than explain — after a subscription changes, say.
    /// A client replaces its state wholesale; it does not merge.
    Snapshot {
        /// The sequence this snapshot is current as of. Events at or below it
        /// are already folded in.
        seq: u64,
        /// The state itself.
        state: S,
    },
    /// One thing that happened.
    Event {
        /// Monotonic per connection. A gap means the client missed something
        /// and must ask for a snapshot rather than carrying on — see
        /// [`Cursor`].
        seq: u64,
        /// The fact.
        event: E,
    },
    /// An action was refused, with something a person could read.
    Refused {
        /// Why.
        reason: String,
    },
    /// Answer to [`Up::Ping`].
    Pong,
}

/// Tracks event ordering on the receiving side.
///
/// The server numbers what it sends **per connection**, so a client can tell
/// "I missed one" from "these arrived out of order". Detecting a gap is the
/// point: the fix is a fresh snapshot, and a client that silently applied
/// event 7 after missing 6 would look fine and be wrong.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    seen: u64,
}

/// What a [`Cursor`] says to do with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// In order. Apply it.
    Apply,
    /// Already applied — a resnapshot overlapped a live event. Ignore it.
    Duplicate,
    /// Something was missed. The state is no longer trustworthy; ask for a
    /// snapshot instead of applying this.
    Gap,
}

impl Cursor {
    /// Start from a snapshot taken at `seq`.
    pub fn at(seq: u64) -> Cursor {
        Cursor { seen: seq }
    }

    /// The last sequence folded in.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Classify an incoming event, advancing on the ones worth applying.
    pub fn step(&mut self, seq: u64) -> Step {
        if seq <= self.seen {
            Step::Duplicate
        } else if seq == self.seen + 1 {
            self.seen = seq;
            Step::Apply
        } else {
            Step::Gap
        }
    }
}

/// Where the server's state actually lives.
///
/// Abstract because the answer differs per field and per deployment: presence
/// belongs in something shared and expiring, messages in Postgres, a typing
/// indicator in nothing at all. The reducer does not care, and this trait is
/// what keeps it from having to.
///
/// Implementations are free to be a plain `Mutex<State>` for a single process
/// — [`hub::Memory`] is exactly that — and the contract is written so a Redis
/// or Postgres implementation can satisfy it without changing callers.
pub trait Store<M: Model>: Send + Sync + 'static {
    /// The state as this connection may see it.
    ///
    /// Scoped by the same topics the connection subscribes to, so a snapshot
    /// and the events that follow it describe exactly the same world. Deriving
    /// visibility twice — once for the snapshot, once for fan-out — is how a
    /// client ends up holding something it will never be told has changed.
    ///
    /// Synchronous on purpose. A store that had to await here would force the
    /// snapshot to be taken *before* the connection is registered, reopening
    /// the window where an event lands between the two. Whatever awaiting the
    /// app needs — reading membership, loading rows — belongs at authorization
    /// time, and arrives here as `topics`.
    fn snapshot(&self, viewer: &str, topics: &[Topic]) -> M::State;

    /// Fold an event into the durable side, if this store has one.
    ///
    /// Called once per event on the server, before it is published. A store
    /// backed by a database may make this a no-op when the action handler has
    /// already written the row — the event is then only a notification.
    fn commit(&self, event: &M::Event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_applies_in_order() {
        let mut c = Cursor::at(0);
        assert_eq!(c.step(1), Step::Apply);
        assert_eq!(c.step(2), Step::Apply);
        assert_eq!(c.seen(), 2);
    }

    /// A resnapshot can overlap events already in flight; folding one twice
    /// must be a no-op decided here, not left to every reducer.
    #[test]
    fn an_event_already_in_the_snapshot_is_a_duplicate() {
        let mut c = Cursor::at(5);
        assert_eq!(c.step(3), Step::Duplicate);
        assert_eq!(c.step(5), Step::Duplicate);
        assert_eq!(c.seen(), 5);
    }

    /// The case the sequence numbers exist for. Applying 7 after missing 6
    /// would leave a client confidently wrong.
    #[test]
    fn a_missed_event_is_a_gap_and_does_not_advance() {
        let mut c = Cursor::at(5);
        assert_eq!(c.step(7), Step::Gap);
        assert_eq!(c.seen(), 5, "a gap must not be treated as progress");
        // ...and the missing one still applies if it turns up.
        assert_eq!(c.step(6), Step::Apply);
        assert_eq!(c.step(7), Step::Apply);
    }

    #[test]
    fn topics_are_scoped_by_kind() {
        assert_eq!(Topic::space("abc").to_string(), "space:abc");
        assert_eq!(Topic::user("abc").to_string(), "user:abc");
        assert_ne!(Topic::space("abc"), Topic::user("abc"));
    }
}
