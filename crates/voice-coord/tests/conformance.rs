//! One suite, every backend.
//!
//! The whole value of [`RoomStore`] is that a single-node deployment on
//! `MemoryStore` and a fleet on `RedisStore` behave identically. That is only
//! true if it is *tested* identically, so every case below is generated once per
//! implementation by the `conformance!` macro.
//!
//! The Redis cases need a server: set `REDIS_URL` (e.g.
//! `redis://127.0.0.1:6399`). Without it they are skipped with a printed
//! notice — a silent skip would let the backend that actually ships in
//! production rot untested.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voice_coord::memory::MemoryStore;
use voice_coord::{LeaveOutcome, NodeLoad, NodeSpec, RoomStore, StoreError, Ttls};

/// Unique key namespace per test, so the suite can run in parallel against one
/// Redis without tests colliding.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique(tag: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("itest:{tag}:{pid}:{n}")
}

fn node(id: &str, url: &str, capacity: u32) -> NodeSpec {
    NodeSpec { id: id.to_string(), url: url.to_string(), capacity }
}

/// Build a store, or `None` when this backend is unavailable in this env.
type Built = Option<Arc<dyn RoomStore>>;

async fn memory(ttls: Ttls, _tag: &str) -> Built {
    Some(Arc::new(MemoryStore::with_ttls(ttls)))
}

#[cfg(feature = "redis-store")]
async fn redis(ttls: Ttls, tag: &str) -> Built {
    let Ok(url) = std::env::var("REDIS_URL") else {
        eprintln!("SKIP redis conformance: REDIS_URL unset");
        return None;
    };
    let store = voice_coord::redis_store::RedisStore::connect(&url, &unique(tag), ttls)
        .await
        .expect("REDIS_URL is set but unreachable — failing rather than skipping");
    Some(Arc::new(store))
}

#[cfg(not(feature = "redis-store"))]
async fn redis(_ttls: Ttls, _tag: &str) -> Built {
    None
}

#[cfg(feature = "postgres-store")]
async fn postgres(ttls: Ttls, tag: &str) -> Built {
    let Ok(url) = std::env::var("POSTGRES_URL") else {
        eprintln!("SKIP postgres conformance: POSTGRES_URL unset");
        return None;
    };
    // A schema per test keeps the suite parallel against one database.
    let schema = unique(tag).replace([':', '-'], "_");
    let store = voice_coord::postgres_store::PostgresStore::connect_in_schema(&url, &schema, ttls)
        .await
        .expect("POSTGRES_URL is set but unreachable — failing rather than skipping");
    store.migrate().await.expect("migrate");
    Some(Arc::new(store))
}

#[cfg(not(feature = "postgres-store"))]
async fn postgres(_ttls: Ttls, _tag: &str) -> Built {
    None
}

/// Generate the whole suite for one backend constructor.
macro_rules! conformance {
    ($modname:ident, $ctor:path) => {
        mod $modname {
            use super::*;

            /// Store with default TTLs, plus `count` registered idle nodes.
            async fn with_nodes(tag: &str, count: usize) -> Option<Arc<dyn RoomStore>> {
                let store = $ctor(Ttls::default(), tag).await?;
                for i in 0..count {
                    store
                        .register_node(node(
                            &format!("n{i}"),
                            &format!("ws://node{i}.test:8787"),
                            0,
                        ))
                        .await
                        .unwrap();
                }
                Some(store)
            }

            #[tokio::test]
            async fn join_assigns_a_node_and_starts_the_count_at_one() {
                let Some(s) = with_nodes("assign", 1).await else { return };
                let a = s.join("r").await.unwrap();
                assert_eq!(a.node_id, "n0");
                assert_eq!(a.node_url, "ws://node0.test:8787");
                assert_eq!(a.participants, 1);
                assert!(a.newly_assigned);
                assert!(a.epoch >= 1, "epoch must start at 1, got {}", a.epoch);
            }

            #[tokio::test]
            async fn everyone_in_a_room_lands_on_the_same_node() {
                // The property a 3-way room depends on. If two participants are
                // assigned different nodes they simply cannot hear each other,
                // and nothing anywhere reports an error.
                let Some(s) = with_nodes("same-node", 4).await else { return };
                let a = s.join("r").await.unwrap();
                let b = s.join("r").await.unwrap();
                let c = s.join("r").await.unwrap();
                assert_eq!(a.node_id, b.node_id);
                assert_eq!(b.node_id, c.node_id);
                assert_eq!(a.epoch, c.epoch, "one room, one assignment generation");
                assert_eq!((a.participants, b.participants, c.participants), (1, 2, 3));
                assert!(!b.newly_assigned && !c.newly_assigned);
            }

            #[tokio::test]
            async fn concurrent_cold_joins_never_split_the_room() {
                // The race the whole crate exists for: N callers hit an empty
                // room at once. A get-then-set implementation lets several
                // "win" and assigns the room to several nodes at once.
                let Some(s) = with_nodes("thunder", 5).await else { return };
                let mut tasks = Vec::new();
                for _ in 0..24 {
                    let s = s.clone();
                    tasks.push(tokio::spawn(async move { s.join("hot").await.unwrap() }));
                }
                let mut results = Vec::new();
                for t in tasks {
                    results.push(t.await.unwrap());
                }

                let nodes: std::collections::HashSet<_> =
                    results.iter().map(|a| a.node_id.clone()).collect();
                assert_eq!(nodes.len(), 1, "room split across nodes: {nodes:?}");

                let epochs: std::collections::HashSet<_> =
                    results.iter().map(|a| a.epoch).collect();
                assert_eq!(epochs.len(), 1, "one room must have one epoch: {epochs:?}");

                assert_eq!(
                    results.iter().filter(|a| a.newly_assigned).count(),
                    1,
                    "exactly one caller may create the room"
                );

                // Counts must be a clean 1..=24 with no duplicates: a duplicate
                // means two increments observed the same prior value.
                let mut counts: Vec<u32> = results.iter().map(|a| a.participants).collect();
                counts.sort_unstable();
                assert_eq!(counts, (1..=24).collect::<Vec<u32>>());
                assert_eq!(s.room("hot").await.unwrap().unwrap().participants, 24);
            }

            #[tokio::test]
            async fn leave_keeps_the_room_open_while_anyone_remains() {
                let Some(s) = with_nodes("partial", 1).await else { return };
                s.join("r").await.unwrap();
                s.join("r").await.unwrap();
                match s.leave("r").await.unwrap() {
                    LeaveOutcome::Active { node_id, participants } => {
                        assert_eq!(node_id, "n0");
                        assert_eq!(participants, 1);
                    }
                    other => panic!("expected Active, got {other:?}"),
                }
                assert!(s.room("r").await.unwrap().is_some());
            }

            #[tokio::test]
            async fn the_last_leave_closes_the_room_and_clears_the_assignment() {
                let Some(s) = with_nodes("cleanup", 1).await else { return };
                s.join("r").await.unwrap();
                assert_eq!(
                    s.leave("r").await.unwrap(),
                    LeaveOutcome::Closed { node_id: "n0".into() }
                );
                assert!(
                    s.room("r").await.unwrap().is_none(),
                    "an empty room must leave nothing behind"
                );
            }

            #[tokio::test]
            async fn a_reopened_room_gets_a_fresh_epoch() {
                // Fencing: a node partitioned away during the room's first life
                // must not be able to serve its second life.
                let Some(s) = with_nodes("epoch", 1).await else { return };
                let first = s.join("r").await.unwrap();
                s.leave("r").await.unwrap();
                let second = s.join("r").await.unwrap();
                assert!(
                    second.epoch > first.epoch,
                    "epoch must advance across close/reopen: {} -> {}",
                    first.epoch,
                    second.epoch
                );
            }

            #[tokio::test]
            async fn a_duplicate_leave_is_idempotent_and_never_wraps() {
                // A retry or a double-disconnect must not drive the count
                // negative — on unsigned counters that wraps to a huge number
                // and pins the room open forever.
                let Some(s) = with_nodes("dup-leave", 1).await else { return };
                s.join("r").await.unwrap();
                s.leave("r").await.unwrap();
                assert_eq!(s.leave("r").await.unwrap(), LeaveOutcome::Unknown);
                assert!(s.room("r").await.unwrap().is_none());

                // And the room still works afterwards.
                let again = s.join("r").await.unwrap();
                assert_eq!(again.participants, 1);
            }

            #[tokio::test]
            async fn no_registered_node_is_reported_as_no_capacity() {
                // The signal to scale out. It must be a distinct error, not a
                // panic and not a silent empty assignment.
                let Some(s) = with_nodes("empty-fleet", 0).await else { return };
                match s.join("r").await {
                    Err(StoreError::NoCapacity) => {}
                    other => panic!("expected NoCapacity, got {other:?}"),
                }
            }

            #[tokio::test]
            async fn a_draining_node_keeps_its_rooms_but_receives_no_new_ones() {
                // How a node is retired without dropping a live call.
                let Some(s) = $ctor(Ttls::default(), "drain").await else { return };
                s.register_node(node("old", "ws://old:8787", 0)).await.unwrap();
                let existing = s.join("keep-me").await.unwrap();
                assert_eq!(existing.node_id, "old");

                s.register_node(node("new", "ws://new:8787", 0)).await.unwrap();
                s.set_draining("old", true).await.unwrap();

                // A new room avoids the draining node...
                assert_eq!(s.join("fresh").await.unwrap().node_id, "new");
                // ...while the existing room keeps working on it.
                let rejoin = s.join("keep-me").await.unwrap();
                assert_eq!(rejoin.node_id, "old");
                assert_eq!(rejoin.participants, 2);
            }

            #[tokio::test]
            async fn capacity_is_enforced_at_assignment_time() {
                let Some(s) = $ctor(Ttls::default(), "cap").await else { return };
                s.register_node(node("tiny", "ws://tiny:8787", 2)).await.unwrap();
                s.join("a").await.unwrap();
                s.join("b").await.unwrap();
                // Full: a NEW room cannot be placed...
                match s.join("c").await {
                    Err(StoreError::NoCapacity) => {}
                    other => panic!("expected NoCapacity at capacity, got {other:?}"),
                }
                // ...but an already-open room still admits people, because
                // refusing there would strand a caller whose peers are already
                // connected and talking.
                assert_eq!(s.join("a").await.unwrap().participants, 2);
            }

            #[tokio::test]
            async fn new_rooms_go_to_the_least_loaded_node() {
                let Some(s) = with_nodes("balance", 3).await else { return };
                let mut placed = std::collections::HashMap::new();
                for i in 0..3 {
                    let a = s.join(&format!("room{i}")).await.unwrap();
                    *placed.entry(a.node_id).or_insert(0) += 1;
                }
                assert_eq!(placed.len(), 3, "three rooms should spread over three nodes: {placed:?}");
            }

            #[tokio::test]
            async fn a_node_that_stops_heartbeating_is_evicted_and_its_room_reassigned() {
                // The crash path. Without this a room is pinned to a corpse and
                // every future joiner is sent to an address that will not answer.
                let ttls = Ttls { node_ms: 300, room_ms: 60_000 };
                let Some(s) = $ctor(ttls, "reap").await else { return };
                s.register_node(node("doomed", "ws://doomed:8787", 0)).await.unwrap();
                let first = s.join("r").await.unwrap();
                assert_eq!(first.node_id, "doomed");

                s.register_node(node("healthy", "ws://healthy:8787", 0)).await.unwrap();
                // Only the healthy node keeps checking in.
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    s.heartbeat("healthy", NodeLoad::default()).await.unwrap();
                }

                assert!(
                    !s.heartbeat("doomed", NodeLoad::default()).await.unwrap(),
                    "a lapsed node must be told it is no longer registered"
                );
                let after = s.join("r").await.unwrap();
                assert_eq!(after.node_id, "healthy", "room must move off the dead node");
                assert!(after.newly_assigned);
                assert!(after.epoch > first.epoch, "reassignment must fence the old owner");
            }

            #[tokio::test]
            async fn removing_a_node_reassigns_its_rooms_on_next_join() {
                let Some(s) = with_nodes("remove", 2).await else { return };
                let first = s.join("r").await.unwrap();
                s.register_node(node("spare", "ws://spare:8787", 0)).await.unwrap();
                s.remove_node(&first.node_id).await.unwrap();

                let after = s.join("r").await.unwrap();
                assert_ne!(after.node_id, first.node_id);
                assert!(after.epoch > first.epoch);
            }

            #[tokio::test]
            async fn concurrent_joins_and_leaves_leave_no_orphan() {
                // Stress the exact interleaving the crate is built around. The
                // invariant is total: at the end, participants and the room
                // record must agree, whichever order the operations landed in.
                let Some(s) = with_nodes("churn", 3).await else { return };
                for _ in 0..8 {
                    let mut tasks = Vec::new();
                    for _ in 0..10 {
                        let s = s.clone();
                        tasks.push(tokio::spawn(async move { s.join("churn").await.unwrap() }));
                    }
                    for t in tasks {
                        t.await.unwrap();
                    }

                    let mut tasks = Vec::new();
                    for _ in 0..10 {
                        let s = s.clone();
                        tasks.push(tokio::spawn(async move { s.leave("churn").await.unwrap() }));
                    }
                    let outcomes: Vec<_> = {
                        let mut v = Vec::new();
                        for t in tasks {
                            v.push(t.await.unwrap());
                        }
                        v
                    };
                    assert_eq!(
                        outcomes.iter().filter(|o| matches!(o, LeaveOutcome::Closed { .. })).count(),
                        1,
                        "exactly one leave may close the room"
                    );
                    assert!(
                        s.room("churn").await.unwrap().is_none(),
                        "room must be fully cleaned up after equal joins and leaves"
                    );
                }
            }

            #[tokio::test]
            async fn heartbeat_from_an_unknown_node_reports_not_registered() {
                // Tells a restarted or evicted node to re-register instead of
                // assuming it is still in the pool and waiting for traffic that
                // will never be routed to it.
                let Some(s) = with_nodes("hb", 0).await else { return };
                assert!(!s.heartbeat("ghost", NodeLoad::default()).await.unwrap());
            }

            #[tokio::test]
            async fn registering_again_does_not_clear_draining() {
                // A node restarting mid-drain must not quietly rejoin the pool.
                let Some(s) = $ctor(Ttls::default(), "redrain").await else { return };
                s.register_node(node("a", "ws://a:8787", 0)).await.unwrap();
                s.register_node(node("b", "ws://b:8787", 0)).await.unwrap();
                s.set_draining("a", true).await.unwrap();
                s.register_node(node("a", "ws://a:8787", 0)).await.unwrap();
                assert_eq!(s.join("r").await.unwrap().node_id, "b");
            }

            #[tokio::test]
            async fn list_nodes_reports_the_live_fleet() {
                let Some(s) = with_nodes("list", 3).await else { return };
                s.heartbeat("n1", NodeLoad { rooms: 2, participants: 7 }).await.unwrap();
                let nodes = s.list_nodes().await.unwrap();
                assert_eq!(nodes.len(), 3);
                let n1 = nodes.iter().find(|n| n.spec.id == "n1").expect("n1 listed");
                assert_eq!(n1.load.participants, 7);
                assert_eq!(n1.load.rooms, 2);
            }
        }
    };
}

conformance!(memory_store, memory);
conformance!(redis_store, redis);
conformance!(postgres_store, postgres);
