//! Proof that the concurrency tests can actually fail.
//!
//! A test that passes against a broken implementation is worse than no test: it
//! converts an unverified assumption into a false guarantee. So this file builds
//! the naive store deliberately — read, then write, with an await point in
//! between, exactly as a `get`/`set` cache forces you to — and asserts the
//! invariants BREAK.
//!
//! If a change to the suite ever makes these pass, the suite has stopped
//! testing atomicity and the failure here says so.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use voice_coord::{
    Assignment, LeaveOutcome, NodeLoad, NodeSpec, NodeView, RoomStore, RoomView, StoreError,
};

#[derive(Default)]
struct Inner {
    nodes: Vec<NodeSpec>,
    rooms: HashMap<String, (String, u64, u32)>,
    next_node: usize,
}

/// A store built the way you would if the backend only offered get and set:
/// read the current state, yield, then write it back.
struct NaiveStore {
    inner: Mutex<Inner>,
}

#[async_trait]
impl RoomStore for NaiveStore {
    async fn register_node(&self, spec: NodeSpec) -> Result<(), StoreError> {
        self.inner.lock().await.nodes.push(spec);
        Ok(())
    }
    async fn heartbeat(&self, _: &str, _: NodeLoad) -> Result<bool, StoreError> {
        Ok(true)
    }
    async fn set_draining(&self, _: &str, _: bool) -> Result<(), StoreError> {
        Ok(())
    }
    async fn remove_node(&self, _: &str) -> Result<(), StoreError> {
        Ok(())
    }
    async fn list_nodes(&self) -> Result<Vec<NodeView>, StoreError> {
        Ok(Vec::new())
    }

    async fn join(&self, room: &str) -> Result<Assignment, StoreError> {
        // READ
        let existing = { self.inner.lock().await.rooms.get(room).cloned() };

        // The gap. In a real get/set backend this is the network round trip
        // between reading the assignment and writing the new count.
        tokio::task::yield_now().await;

        // MODIFY + WRITE
        let mut g = self.inner.lock().await;
        match existing {
            Some((node_id, epoch, count)) => {
                let count = count + 1;
                g.rooms.insert(room.to_string(), (node_id.clone(), epoch, count));
                let url = g
                    .nodes
                    .iter()
                    .find(|n| n.id == node_id)
                    .map(|n| n.url.clone())
                    .unwrap_or_default();
                Ok(Assignment {
                    node_id,
                    node_url: url,
                    epoch,
                    participants: count,
                    newly_assigned: false,
                })
            }
            None => {
                if g.nodes.is_empty() {
                    return Err(StoreError::NoCapacity);
                }
                let idx = g.next_node % g.nodes.len();
                g.next_node += 1;
                let spec = g.nodes[idx].clone();
                g.rooms.insert(room.to_string(), (spec.id.clone(), 1, 1));
                Ok(Assignment {
                    node_id: spec.id,
                    node_url: spec.url,
                    epoch: 1,
                    participants: 1,
                    newly_assigned: true,
                })
            }
        }
    }

    async fn leave(&self, room: &str) -> Result<LeaveOutcome, StoreError> {
        let existing = { self.inner.lock().await.rooms.get(room).cloned() };
        tokio::task::yield_now().await;
        let mut g = self.inner.lock().await;
        match existing {
            None => Ok(LeaveOutcome::Unknown),
            Some((node_id, epoch, count)) => {
                let remaining = count.saturating_sub(1);
                if remaining == 0 {
                    g.rooms.remove(room);
                    Ok(LeaveOutcome::Closed { node_id })
                } else {
                    g.rooms.insert(room.to_string(), (node_id.clone(), epoch, remaining));
                    Ok(LeaveOutcome::Active { node_id, participants: remaining })
                }
            }
        }
    }

    async fn room(&self, room: &str) -> Result<Option<RoomView>, StoreError> {
        Ok(self.inner.lock().await.rooms.get(room).map(|(n, e, c)| RoomView {
            room: room.to_string(),
            node_id: n.clone(),
            epoch: *e,
            participants: *c,
        }))
    }
}

async fn naive_with_nodes(count: usize) -> Arc<dyn RoomStore> {
    let store = Arc::new(NaiveStore { inner: Mutex::new(Inner::default()) });
    for i in 0..count {
        store
            .register_node(NodeSpec {
                id: format!("n{i}"),
                url: format!("ws://node{i}:8787"),
                capacity: 0,
            })
            .await
            .unwrap();
    }
    store
}

#[tokio::test]
async fn naive_store_splits_a_room_across_nodes() {
    // The production failure this guards against: several people "in the same
    // room" placed on different nodes, hearing nothing, with no error raised.
    let store = naive_with_nodes(5).await;
    let mut tasks = Vec::new();
    for _ in 0..24 {
        let s = store.clone();
        tasks.push(tokio::spawn(async move { s.join("hot").await.unwrap() }));
    }
    let mut nodes = std::collections::HashSet::new();
    let mut creators = 0;
    for t in tasks {
        let a = t.await.unwrap();
        nodes.insert(a.node_id);
        if a.newly_assigned {
            creators += 1;
        }
    }

    assert!(
        nodes.len() > 1 || creators > 1,
        "the naive store was expected to split the room or double-create it, but it \
         behaved atomically (nodes={}, creators={}). If this ever holds, the \
         concurrency suite is no longer proving anything.",
        nodes.len(),
        creators
    );
}

#[tokio::test]
async fn naive_store_loses_participant_counts() {
    // Lost updates: two increments read the same prior value, so the room's
    // count under-reports and it closes while people are still connected.
    let store = naive_with_nodes(1).await;
    let mut tasks = Vec::new();
    for _ in 0..24 {
        let s = store.clone();
        tasks.push(tokio::spawn(async move { s.join("r").await.unwrap() }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let counted = store.room("r").await.unwrap().unwrap().participants;
    assert!(
        counted < 24,
        "expected lost updates from read-modify-write, but the count was exact ({counted}/24)"
    );
}
