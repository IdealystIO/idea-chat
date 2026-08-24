//! The server side: who is connected, what they can see, and fan-out.
//!
//! One [`Hub`] per model per process. It knows nothing about the app's types
//! beyond [`Model`], and nothing about the transport — a connection hands it a
//! subscription and reads frames off a channel.
//!
//! # Ordering
//!
//! Sequence numbers are **per connection**, assigned as frames are handed out.
//! Not global: a global counter would make every client's cursor jump by the
//! traffic of everyone else, so a gap would stop meaning "you missed one".
//! Per connection, the numbers are dense, and a gap is a real fault.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::{Down, Model, Store, Topic};

/// How many frames may queue for one connection before it is considered stuck.
///
/// A slow client must not be allowed to hold the state of the world in memory
/// on the server, and it must not slow anyone else down. When this fills, the
/// connection is dropped rather than blocked — it reconnects and takes a fresh
/// snapshot, which is both cheaper and more correct than a client applying a
/// stale backlog.
const BACKLOG: usize = 256;

/// One connected client.
struct Conn<M: Model> {
    tx: mpsc::Sender<Down<M::State, M::Event>>,
    /// Per-connection sequence. See the module note on why not global.
    seq: AtomicU64,
    /// What this connection is allowed to receive. Set when it is authorized;
    /// an event on a topic not in here is not sent, which is the whole of the
    /// visibility rule.
    topics: Vec<Topic>,
}

/// A connection's read half, plus the identity the hub holds it under.
pub struct Subscription<M: Model> {
    /// Frames for this client, in order.
    pub rx: mpsc::Receiver<Down<M::State, M::Event>>,
    id: u64,
    hub: Hub<M>,
}

impl<M: Model> Drop for Subscription<M> {
    /// Unsubscribing on drop rather than requiring a call: the transport has
    /// many ways to end (a clean close, an error, a task cancelled mid-await)
    /// and only one of them is a path anybody remembers to write cleanup on.
    fn drop(&mut self) {
        self.hub.remove(self.id);
    }
}

/// The fan-out point.
///
/// Cheap to clone — every clone is the same hub.
pub struct Hub<M: Model> {
    inner: Arc<Inner<M>>,
}

impl<M: Model> Clone for Hub<M> {
    fn clone(&self) -> Self {
        Hub { inner: self.inner.clone() }
    }
}

struct Inner<M: Model> {
    conns: Mutex<HashMap<u64, Conn<M>>>,
    next_id: AtomicU64,
    store: Box<dyn Store<M>>,
}

impl<M: Model> Hub<M> {
    /// Build a hub over a store.
    pub fn new(store: impl Store<M>) -> Hub<M> {
        Hub {
            inner: Arc::new(Inner {
                conns: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                store: Box::new(store),
            }),
        }
    }

    /// Attach a client, and hand it the snapshot it starts from.
    ///
    /// The snapshot is queued **inside the lock that registers the
    /// connection**, so no event can slip between "this client exists" and
    /// "this client has a starting point". Without that, a join happening at
    /// exactly the wrong moment is either lost (published before registration)
    /// or applied to a state that already contains it — and the second is only
    /// harmless because [`Model::apply`] is required to tolerate it.
    pub fn connect(&self, viewer: &str, topics: Vec<Topic>) -> Subscription<M> {
        let (tx, rx) = mpsc::channel(BACKLOG);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);

        let mut conns = self.inner.conns.lock().expect("hub lock");
        let state = self.inner.store.snapshot(viewer, &topics);
        // Cannot fail: nothing else holds this sender yet and the channel is
        // empty.
        let _ = tx.try_send(Down::Snapshot { seq: 0, state });
        conns.insert(id, Conn { tx, seq: AtomicU64::new(0), topics });
        drop(conns);

        Subscription { rx, id, hub: self.clone() }
    }

    /// Commit an event and send it to everyone subscribed to `topic`.
    ///
    /// Returns how many connections it reached, which is only useful for
    /// tests and logs — an event is true whether or not anyone was listening.
    pub fn publish(&self, topic: &Topic, event: M::Event) -> usize {
        self.inner.store.commit(&event);

        let mut sent = 0;
        let mut stuck = Vec::new();
        {
            let conns = self.inner.conns.lock().expect("hub lock");
            for (id, conn) in conns.iter() {
                if !conn.topics.contains(topic) {
                    continue;
                }
                let seq = conn.seq.fetch_add(1, Ordering::Relaxed) + 1;
                match conn.tx.try_send(Down::Event { seq, event: event.clone() }) {
                    Ok(()) => sent += 1,
                    // Full or closed: either way this connection is not
                    // keeping up and is better off reconnecting into a fresh
                    // snapshot than being waited for.
                    Err(_) => stuck.push(*id),
                }
            }
        }
        for id in stuck {
            self.remove(id);
        }
        sent
    }

    /// The state as `viewer` sees it, without connecting.
    ///
    /// The same call [`connect`](Self::connect) makes, exposed so a plain HTTP
    /// request can serve the identical snapshot — one definition of "what you
    /// can see", whether it arrives over a socket or a fetch.
    pub fn snapshot(&self, viewer: &str, topics: &[Topic]) -> M::State {
        self.inner.store.snapshot(viewer, topics)
    }

    /// How many clients are attached. For tests and operators.
    pub fn connections(&self) -> usize {
        self.inner.conns.lock().expect("hub lock").len()
    }

    fn remove(&self, id: u64) {
        self.inner.conns.lock().expect("hub lock").remove(&id);
    }
}

/// A [`Store`] that keeps everything in this process's memory.
///
/// Correct for a single-instance deployment and for tests. It is deliberately
/// the simplest thing that satisfies the trait, so the trait's shape is proven
/// by something other than the real implementation.
pub struct Memory<M: Model> {
    state: Mutex<M::State>,
}

impl<M: Model> Memory<M> {
    /// Start from an initial state.
    pub fn new(initial: M::State) -> Memory<M> {
        Memory { state: Mutex::new(initial) }
    }
}

impl<M: Model> Store<M> for Memory<M>
where
    M::State: Clone + Send + Sync + 'static,
{
    /// Everyone sees the same thing. A real store scopes by viewer and topic.
    fn snapshot(&self, _viewer: &str, _topics: &[Topic]) -> M::State {
        self.state.lock().expect("state lock").clone()
    }

    fn commit(&self, event: &M::Event) {
        M::apply(&mut self.state.lock().expect("state lock"), event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Default, PartialEq)]
    struct Counter {
        n: i64,
        who: Vec<String>,
    }

    #[derive(Clone, Debug)]
    enum Ev {
        Added(i64),
        Joined(String),
    }

    struct M;
    impl Model for M {
        type State = Counter;
        type Event = Ev;
        type Action = ();
        fn apply(s: &mut Counter, e: &Ev) {
            match e {
                Ev::Added(n) => s.n += n,
                // Tolerating a repeat, as the trait requires.
                Ev::Joined(w) => {
                    if !s.who.contains(w) {
                        s.who.push(w.clone());
                    }
                }
            }
        }
    }

    fn hub() -> Hub<M> {
        Hub::new(Memory::<M>::new(Counter::default()))
    }

    #[tokio::test]
    async fn a_new_connection_is_given_the_state_it_starts_from() {
        let hub = hub();
        hub.publish(&Topic::space("s"), Ev::Added(3));

        let mut sub = hub.connect("ana", vec![Topic::space("s")]);
        match sub.rx.recv().await.expect("a snapshot") {
            Down::Snapshot { state, .. } => assert_eq!(state.n, 3),
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn events_reach_subscribers_and_are_numbered_densely() {
        let hub = hub();
        let mut sub = hub.connect("ana", vec![Topic::space("s")]);
        let _ = sub.rx.recv().await; // the snapshot

        hub.publish(&Topic::space("s"), Ev::Added(1));
        hub.publish(&Topic::space("s"), Ev::Added(1));

        for expected in 1..=2u64 {
            match sub.rx.recv().await.expect("an event") {
                Down::Event { seq, .. } => assert_eq!(seq, expected),
                other => panic!("expected an event, got {other:?}"),
            }
        }
    }

    /// The visibility rule. A connection that never subscribed to a topic must
    /// not receive it — this is the difference between a live update and a
    /// leak.
    #[tokio::test]
    async fn an_event_on_another_topic_is_not_delivered() {
        let hub = hub();
        let mut sub = hub.connect("ana", vec![Topic::space("mine")]);
        let _ = sub.rx.recv().await;

        assert_eq!(hub.publish(&Topic::space("theirs"), Ev::Added(1)), 0);
        assert_eq!(hub.publish(&Topic::space("mine"), Ev::Added(1)), 1);

        // The first frame waiting is the one for "mine", not "theirs".
        match sub.rx.recv().await.expect("an event") {
            Down::Event { seq, .. } => assert_eq!(seq, 1, "a foreign event was numbered"),
            other => panic!("{other:?}"),
        }
    }

    /// Sequence numbers are per connection, so a second client's numbering
    /// starts at one regardless of what the first has seen.
    #[tokio::test]
    async fn each_connection_is_numbered_from_its_own_start() {
        let hub = hub();
        let mut first = hub.connect("ana", vec![Topic::space("s")]);
        let _ = first.rx.recv().await;
        hub.publish(&Topic::space("s"), Ev::Added(1));
        hub.publish(&Topic::space("s"), Ev::Added(1));

        let mut second = hub.connect("theo", vec![Topic::space("s")]);
        match second.rx.recv().await.expect("a snapshot") {
            // Late joiner: the two events are already in its snapshot...
            Down::Snapshot { state, .. } => assert_eq!(state.n, 2),
            other => panic!("{other:?}"),
        }
        hub.publish(&Topic::space("s"), Ev::Added(1));
        match second.rx.recv().await.expect("an event") {
            // ...and its first event is seq 1, not seq 3.
            Down::Event { seq, .. } => assert_eq!(seq, 1),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn dropping_a_subscription_detaches_it() {
        let hub = hub();
        let sub = hub.connect("ana", vec![Topic::space("s")]);
        assert_eq!(hub.connections(), 1);
        drop(sub);
        assert_eq!(hub.connections(), 0);
        assert_eq!(hub.publish(&Topic::space("s"), Ev::Added(1)), 0);
    }

    /// A client that stops reading must not be able to hold the server's
    /// memory hostage, and must not slow down anyone else.
    #[tokio::test]
    async fn a_client_that_stops_reading_is_dropped_rather_than_waited_for() {
        let hub = hub();
        let _slow = hub.connect("ana", vec![Topic::space("s")]);
        let mut ok = hub.connect("theo", vec![Topic::space("s")]);
        let _ = ok.rx.recv().await;

        // Overfill the slow client's queue; the healthy one keeps up.
        for _ in 0..(BACKLOG + 8) {
            hub.publish(&Topic::space("s"), Ev::Added(1));
            let _ = ok.rx.try_recv();
        }
        assert_eq!(hub.connections(), 1, "the stuck connection was not dropped");
    }

    #[tokio::test]
    async fn the_snapshot_reflects_committed_events() {
        let hub = hub();
        hub.publish(&Topic::space("s"), Ev::Joined("ana".into()));
        hub.publish(&Topic::space("s"), Ev::Joined("ana".into()));
        assert_eq!(hub.snapshot("anyone", &[]).who, vec!["ana".to_string()]);
    }
}
