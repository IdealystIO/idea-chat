//! In-process implementation — dev, tests, and single-node deployments.
//!
//! Atomicity comes from holding one mutex across each compound operation. That
//! is exactly the guarantee the Redis backend gets from running a Lua script on
//! Redis's single-threaded core, which is what lets one conformance suite prove
//! both.
//!
//! This is a real implementation, not a mock: a single-node deployment needs no
//! Redis at all, and gets identical semantics.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;

use crate::{
    Assignment, Epoch, LeaveOutcome, NodeLoad, NodeSpec, NodeView, RoomStore, RoomView, StoreError,
    Ttls,
};

struct NodeEntry {
    spec: NodeSpec,
    load: NodeLoad,
    draining: bool,
    last_seen: Instant,
}

struct RoomEntry {
    node_id: String,
    epoch: Epoch,
    participants: u32,
}

#[derive(Default)]
struct State {
    nodes: HashMap<String, NodeEntry>,
    rooms: HashMap<String, RoomEntry>,
    /// Per-room epoch counter. Deliberately OUTLIVES the room: reusing an epoch
    /// after a room closes and reopens would let a zombie node's stale claim
    /// look current again, which is the whole thing fencing prevents.
    epochs: HashMap<String, Epoch>,
}

/// In-process [`RoomStore`].
pub struct MemoryStore {
    state: Mutex<State>,
    ttls: Ttls,
}

impl MemoryStore {
    /// A store with default TTLs.
    pub fn new() -> Self {
        Self::with_ttls(Ttls::default())
    }

    /// A store with explicit TTLs — tests use short ones to exercise expiry.
    pub fn with_ttls(ttls: Ttls) -> Self {
        MemoryStore { state: Mutex::new(State::default()), ttls }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned mutex means a previous holder panicked mid-operation and
        // the invariant may be broken. Recovering the guard and continuing
        // would silently serve inconsistent assignments, so fail loudly.
        self.state.lock().expect("coordination state poisoned")
    }

    /// Drop nodes that have not checked in within the node TTL.
    fn prune(state: &mut State, ttls: &Ttls) {
        let now = Instant::now();
        state.nodes.retain(|_, n| {
            now.duration_since(n.last_seen).as_millis() as u64 <= ttls.node_ms
        });
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RoomStore for MemoryStore {
    async fn register_node(&self, spec: NodeSpec) -> Result<(), StoreError> {
        let mut state = self.lock();
        let entry = state.nodes.entry(spec.id.clone()).or_insert_with(|| NodeEntry {
            spec: spec.clone(),
            load: NodeLoad::default(),
            draining: false,
            last_seen: Instant::now(),
        });
        // Re-registration refreshes identity and liveness but must NOT reset
        // draining: a node restarting mid-drain should stay out of the pool.
        entry.spec = spec;
        entry.last_seen = Instant::now();
        Ok(())
    }

    async fn heartbeat(&self, id: &str, load: NodeLoad) -> Result<bool, StoreError> {
        let mut state = self.lock();
        Self::prune(&mut state, &self.ttls);
        match state.nodes.get_mut(id) {
            Some(node) => {
                node.load = load;
                node.last_seen = Instant::now();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_draining(&self, id: &str, draining: bool) -> Result<(), StoreError> {
        if let Some(node) = self.lock().nodes.get_mut(id) {
            node.draining = draining;
        }
        Ok(())
    }

    async fn remove_node(&self, id: &str) -> Result<(), StoreError> {
        let mut state = self.lock();
        state.nodes.remove(id);
        // Rooms owned by the node are deliberately LEFT in place: `join`
        // already detects an owner that is no longer registered and reassigns.
        // Reusing that one path keeps both backends identical — Redis cannot
        // find rooms by node without a second index that would itself need to
        // be kept atomic, and a second teardown path is a second thing to get
        // subtly wrong.
        Ok(())
    }

    async fn list_nodes(&self) -> Result<Vec<NodeView>, StoreError> {
        let mut state = self.lock();
        Self::prune(&mut state, &self.ttls);
        let now = Instant::now();
        Ok(state
            .nodes
            .values()
            .map(|n| NodeView {
                spec: n.spec.clone(),
                load: n.load,
                draining: n.draining,
                last_seen_ms_ago: now.duration_since(n.last_seen).as_millis() as u64,
            })
            .collect())
    }

    async fn join(&self, room: &str) -> Result<Assignment, StoreError> {
        let mut state = self.lock();
        Self::prune(&mut state, &self.ttls);

        // Reuse an open room, but only if its node is still alive. A room whose
        // owner has been pruned is stale and must be reassigned, not handed out.
        if let Some(existing) = state.rooms.get(room) {
            if state.nodes.contains_key(&existing.node_id) {
                let node_id = existing.node_id.clone();
                let epoch = existing.epoch;
                let entry = state.rooms.get_mut(room).expect("checked above");
                entry.participants += 1;
                let participants = entry.participants;
                let node = state.nodes.get_mut(&node_id).expect("checked above");
                node.load.participants += 1;
                return Ok(Assignment {
                    node_url: node.spec.url.clone(),
                    node_id,
                    epoch,
                    participants,
                    newly_assigned: false,
                });
            }
            state.rooms.remove(room);
        }

        // Least-loaded alive, non-draining, under-capacity node.
        let choice = state
            .nodes
            .values()
            .filter(|n| !n.draining)
            .filter(|n| n.spec.capacity == 0 || n.load.participants < n.spec.capacity)
            // Tie-break by id so an equal-load fleet places rooms identically
            // to the SQL/Lua backends' `ORDER BY participants ASC, id ASC`.
            .min_by_key(|n| (n.load.participants, n.spec.id.clone()))
            .map(|n| (n.spec.id.clone(), n.spec.url.clone()));
        let (node_id, node_url) = choice.ok_or(StoreError::NoCapacity)?;

        let epoch = {
            let slot = state.epochs.entry(room.to_string()).or_insert(0);
            *slot += 1;
            *slot
        };
        state.rooms.insert(
            room.to_string(),
            RoomEntry { node_id: node_id.clone(), epoch, participants: 1 },
        );
        let node = state.nodes.get_mut(&node_id).expect("just chosen");
        node.load.rooms += 1;
        node.load.participants += 1;

        Ok(Assignment { node_id, node_url, epoch, participants: 1, newly_assigned: true })
    }

    async fn leave(&self, room: &str) -> Result<LeaveOutcome, StoreError> {
        let mut state = self.lock();
        let Some(entry) = state.rooms.get_mut(room) else {
            return Ok(LeaveOutcome::Unknown);
        };
        let node_id = entry.node_id.clone();
        // Saturating: a duplicate leave must not wrap to u32::MAX and pin the
        // room open forever.
        entry.participants = entry.participants.saturating_sub(1);
        let remaining = entry.participants;

        if let Some(node) = state.nodes.get_mut(&node_id) {
            node.load.participants = node.load.participants.saturating_sub(1);
        }

        if remaining == 0 {
            state.rooms.remove(room);
            if let Some(node) = state.nodes.get_mut(&node_id) {
                node.load.rooms = node.load.rooms.saturating_sub(1);
            }
            return Ok(LeaveOutcome::Closed { node_id });
        }
        Ok(LeaveOutcome::Active { node_id, participants: remaining })
    }

    async fn room(&self, room: &str) -> Result<Option<RoomView>, StoreError> {
        Ok(self.lock().rooms.get(room).map(|r| RoomView {
            room: room.to_string(),
            node_id: r.node_id.clone(),
            epoch: r.epoch,
            participants: r.participants,
        }))
    }
}
