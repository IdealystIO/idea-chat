//! Wiring a node into the coordination store.
//!
//! # Who counts participants
//!
//! The node does — both directions. `join` runs when a socket opens and `leave`
//! runs when that same socket closes, so every increment has a decrement bound
//! to the same event.
//!
//! The tempting alternative is for the director to increment at placement time.
//! It leaks: a client that is placed and then never connects (crash, closed tab,
//! flaky network) leaves a participant that nothing will ever remove, and the
//! room stays "occupied" and pinned to a node until its TTL sweeps it. Tying the
//! count to the socket makes that unrepresentable.
//!
//! The cost is a placement race: two clients arriving at a cold room can be sent
//! to different nodes by the director, since the director only *hints*. The
//! store settles it — whichever node's `join` lands first owns the room, and the
//! loser tells its client to [`REDIRECT`] to the winner. Rare, self-correcting,
//! and it keeps the store as the only source of truth.

use std::sync::Arc;
use std::time::Duration;

use voice_coord::{Epoch, NodeLoad, NodeSpec, RoomStore};

/// Text frame telling a client it reached the wrong node: `redirect:<ws-url>`.
pub const REDIRECT: &str = "redirect:";

/// How often a node reports liveness. Must be comfortably under
/// `Ttls::node_ms` so an ordinary scheduling hiccup does not evict a healthy
/// node mid-call.
pub const HEARTBEAT_PERIOD: Duration = Duration::from_secs(5);

/// A node's coordination handle.
#[derive(Clone)]
pub struct NodeCoord {
    /// Shared coordination store.
    pub store: Arc<dyn RoomStore>,
    /// This node's identity in the fleet.
    pub spec: NodeSpec,
}

impl NodeCoord {
    /// Register, then heartbeat forever in the background.
    ///
    /// The loop re-registers whenever the store says it no longer knows this
    /// node. That happens after a network partition long enough to lapse the
    /// TTL: without re-registration the node would sit healthy and idle,
    /// invisible to the fleet, and never receive traffic again.
    pub async fn start(&self, load: impl Fn() -> NodeLoad + Send + 'static) {
        if let Err(e) = self.store.register_node(self.spec.clone()).await {
            eprintln!("[coord] initial registration failed: {e}");
        }
        let store = self.store.clone();
        let spec = self.spec.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_PERIOD);
            loop {
                tick.tick().await;
                match store.heartbeat(&spec.id, load()).await {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!("[coord] evicted from the fleet — re-registering");
                        if let Err(e) = store.register_node(spec.clone()).await {
                            eprintln!("[coord] re-registration failed: {e}");
                        }
                    }
                    Err(e) => eprintln!("[coord] heartbeat failed: {e}"),
                }
            }
        });
    }

    /// Mark this node draining so it takes no new rooms while keeping its
    /// current ones. The graceful half of scaling in.
    pub async fn drain(&self) {
        if let Err(e) = self.store.set_draining(&self.spec.id, true).await {
            eprintln!("[coord] could not set draining: {e}");
        }
    }

    /// Leave the fleet entirely. Rooms are reassigned on their next join.
    pub async fn deregister(&self) {
        if let Err(e) = self.store.remove_node(&self.spec.id).await {
            eprintln!("[coord] deregistration failed: {e}");
        }
    }
}

/// Outcome of a socket trying to enter a room on this node.
pub enum Admission {
    /// This node owns the room; serve the socket at this epoch.
    Admitted {
        /// Assignment generation this socket belongs to.
        epoch: Epoch,
        /// Participants after admitting it.
        participants: u32,
    },
    /// Another node owns the room — send the client there.
    Redirect {
        /// URL of the owning node.
        url: String,
    },
    /// Nothing could take the room.
    Rejected {
        /// Human-readable reason, forwarded to the client.
        reason: String,
    },
}
