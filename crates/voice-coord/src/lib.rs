//! Room/node coordination — the shared state a fleet of voice nodes agrees on.
//!
//! # The problem this exists to solve
//!
//! A room lives on exactly one node. Two facts have to move together:
//!
//! - **which node owns the room**, and
//! - **how many participants are in it** (because at zero, the room is closed
//!   and the assignment must go away).
//!
//! If those two ever disagree, the failure is severe and quiet: participants get
//! sent to a node that has torn the room down, or a room is pinned to a node
//! forever with nobody in it, or — worst — two people "in the same room" land on
//! different nodes and simply cannot hear each other.
//!
//! Every one of those is a **read-modify-write race**, so a get/set cache cannot
//! express the fix. (The framework's own `cache` crate says as much: it
//! deliberately excludes atomic read-modify-write.) The trait below is therefore
//! defined in terms of **compound operations** — `join`, `leave` — not
//! primitives. Each is one indivisible step, and every backend must make it so.
//!
//! # Why that makes interleaving safe
//!
//! With `join` and `leave` each atomic, the dangerous interleaving stops being
//! dangerous — both orders are correct:
//!
//! - `join` lands first on a 1-participant room: count 1 -> 2, then `leave`
//!   takes it to 1. Room stays open on the same node. Correct.
//! - `leave` lands first: count 1 -> 0, room closed and assignment dropped; the
//!   `join` then finds no room and assigns fresh. Correct — the leaver really
//!   was the last one out.
//!
//! What is NOT correct, and what a naive `get` + `set` produces, is a `join`
//! that reads the assignment *between* another caller's decrement and delete.
//! It connects to a node that is about to drop the room, and nothing anywhere
//! reports an error.
//!
//! # Failure handling
//!
//! - **A node dies with people in it.** Its counters never decrement, so the
//!   room would be pinned to a corpse. Nodes therefore hold a **TTL-backed
//!   registration refreshed by heartbeat**; when one lapses, the next `join`
//!   prunes it and reassigns the room. The system self-heals without a reaper.
//! - **A zombie node returns.** Every assignment carries a monotonic
//!   [`Epoch`] for the room. A node that resumes with a stale epoch is fenced
//!   off rather than allowed to serve a room that has moved.
//! - **Scaling down.** A node marked draining keeps its existing rooms but is
//!   never chosen for new ones, so it empties out and can be retired without
//!   dropping a live call.
//!
//! # Choosing a backend
//!
//! All three implementations pass one shared conformance suite, so this is an
//! operational decision, not a correctness one.
//!
//! - [`postgres_store`] — **the default recommendation.** A chat product already
//!   runs Postgres for users, channels, and messages. Coordination traffic is
//!   one write per user per room transition — not per audio packet — so even a
//!   busy deployment is a few hundred writes per second, which Postgres does not
//!   notice. Atomicity comes from a per-room advisory lock inside a transaction:
//!   concurrent joins to the SAME room serialize, different rooms never contend.
//!   It is also the only backend where the assignment is durable across a
//!   restart of the coordination tier.
//! - [`redis_store`] — worth it when coordination traffic genuinely outgrows
//!   what you want on the primary database, or when Redis is already deployed
//!   for cache/pubsub. Faster per operation; the cost is a second stateful
//!   system to run, monitor, and fail over.
//! - [`memory`] — single-node deployments and tests. No external dependency at
//!   all, identical semantics.
//!
//! The honest summary: **Redis is an optimization here, not a requirement.**
//! The hard part is atomicity, and Postgres has had that longer than Redis has
//! existed.

pub mod memory;
#[cfg(feature = "postgres-store")]
pub mod postgres_store;
#[cfg(feature = "redis-store")]
pub mod redis_store;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Identifier for a voice node.
pub type NodeId = String;
/// Identifier for a room.
pub type RoomId = String;

/// Monotonic generation for a room's assignment.
///
/// Bumped on every fresh assignment and never reused, so a node that was
/// partitioned away and comes back can be told its claim is stale. Without
/// fencing, a zombie node happily serves a room that has been reassigned, and
/// the room silently splits in two.
pub type Epoch = u64;

/// What a node advertises about itself when registering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    /// Stable id, unique per process.
    pub id: NodeId,
    /// The `ws://` URL clients dial for this node.
    pub url: String,
    /// Maximum participants this node will accept. `0` means unbounded.
    ///
    /// Capacity is enforced at ASSIGNMENT time, which is the only place it can
    /// be enforced without dropping someone mid-call.
    pub capacity: u32,
}

/// Mutable load a node reports on each heartbeat.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct NodeLoad {
    /// Rooms currently hosted.
    pub rooms: u32,
    /// Participants currently connected.
    pub participants: u32,
}

/// A node as the store sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    /// Node identity and dial URL.
    pub spec: NodeSpec,
    /// Last reported load.
    pub load: NodeLoad,
    /// Draining nodes keep their rooms but receive no new ones.
    pub draining: bool,
    /// Milliseconds since this node last checked in.
    pub last_seen_ms_ago: u64,
}

/// The result of a successful [`RoomStore::join`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    /// Node now responsible for the room.
    pub node_id: NodeId,
    /// URL the client should dial.
    pub node_url: String,
    /// Generation of this assignment, for fencing.
    pub epoch: Epoch,
    /// Participant count INCLUDING this joiner.
    pub participants: u32,
    /// True when this call created the room (rather than joining an open one).
    pub newly_assigned: bool,
}

/// The result of a [`RoomStore::leave`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaveOutcome {
    /// The last participant left: the room is closed and the assignment gone.
    Closed {
        /// Node that had been hosting it.
        node_id: NodeId,
    },
    /// Participants remain.
    Active {
        /// Node still hosting it.
        node_id: NodeId,
        /// Remaining participant count.
        participants: u32,
    },
    /// No such room — a duplicate or late leave. Idempotent, not an error:
    /// retries and crash-recovery both produce these, and treating them as
    /// failures makes cleanup paths worse, not safer.
    Unknown,
}

/// A room as the store sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomView {
    /// Room identifier.
    pub room: RoomId,
    /// Node hosting it.
    pub node_id: NodeId,
    /// Current assignment generation.
    pub epoch: Epoch,
    /// Current participant count.
    pub participants: u32,
}

/// Why a coordination call failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// No node is alive, non-draining, and under capacity. The caller should
    /// surface this as "try again shortly" and it is the signal to scale out.
    #[error("no voice node has capacity")]
    NoCapacity,
    /// The backing store failed.
    #[error("coordination backend: {0}")]
    Backend(String),
}

/// How long a node's registration survives without a heartbeat, and how long a
/// room record survives without activity.
#[derive(Debug, Clone, Copy)]
pub struct Ttls {
    /// A node not heard from within this window is treated as dead and pruned.
    ///
    /// This is the outage-detection window: too short and a GC pause evicts a
    /// healthy node mid-call; too long and joins keep being sent to a corpse.
    pub node_ms: u64,
    /// Backstop lifetime for a room record, refreshed on every join/leave.
    ///
    /// This only matters if participant bookkeeping is lost entirely (a node
    /// dies holding the only decrements). It keeps a leak bounded rather than
    /// permanent.
    pub room_ms: u64,
}

impl Default for Ttls {
    fn default() -> Self {
        // 15s tolerates a couple of missed 5s heartbeats without evicting a
        // healthy node; an hour is long enough that a live room never expires
        // under it, short enough that a leak is not forever.
        Ttls { node_ms: 15_000, room_ms: 3_600_000 }
    }
}

/// The coordination contract every backend implements.
///
/// `join` and `leave` MUST each be atomic with respect to concurrent callers.
/// That requirement is the entire reason this trait exists, and the shared
/// conformance suite in `tests/` exercises it against every implementation so
/// they cannot drift apart.
#[async_trait]
pub trait RoomStore: Send + Sync + 'static {
    /// Announce a node as available. Idempotent — re-registering an existing id
    /// refreshes it, which is what a restarted node needs.
    async fn register_node(&self, spec: NodeSpec) -> Result<(), StoreError>;

    /// Refresh a node's liveness and report load. Returns `false` if the store
    /// no longer knows this node (it was pruned), meaning the caller should
    /// re-register rather than assume it is still in the pool.
    async fn heartbeat(&self, id: &str, load: NodeLoad) -> Result<bool, StoreError>;

    /// Mark a node as draining (or not). Draining keeps existing rooms alive
    /// and refuses new assignments — the graceful path for scaling in.
    async fn set_draining(&self, id: &str, draining: bool) -> Result<(), StoreError>;

    /// Remove a node immediately. Rooms it owned become unassigned and are
    /// reassigned on the next join.
    async fn remove_node(&self, id: &str) -> Result<(), StoreError>;

    /// Every live node, for operators and for capacity planning.
    async fn list_nodes(&self) -> Result<Vec<NodeView>, StoreError>;

    /// Add one participant to `room`, assigning a node if it is not open yet.
    ///
    /// Atomic: concurrent joins to a cold room all receive the SAME node.
    async fn join(&self, room: &str) -> Result<Assignment, StoreError>;

    /// Remove one participant, closing the room when it reaches zero.
    ///
    /// Atomic: the decrement and the close are one step, so no join can observe
    /// the assignment during teardown.
    async fn leave(&self, room: &str) -> Result<LeaveOutcome, StoreError>;

    /// Inspect a room without modifying it.
    async fn room(&self, room: &str) -> Result<Option<RoomView>, StoreError>;
}
