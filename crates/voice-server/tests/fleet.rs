//! Fleet behavior: several real nodes, a real store, real sockets.
//!
//! `relay.rs` covers a single node's forwarding rules. This file covers what
//! only appears with more than one node: that a room converges onto ONE of them,
//! that the assignment is released the moment the room empties, that a redirect
//! corrects a bad placement, and that draining takes a node out of rotation
//! without disturbing its live calls.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use voice_coord::memory::MemoryStore;
use voice_coord::{NodeSpec, RoomStore, Ttls};
use voice_proto::Packet;
use voice_server::coord::NodeCoord;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Start a node bound to an ephemeral port, registered in `store`.
async fn spawn_node(store: Arc<dyn RoomStore>, id: &str, capacity: u32) -> (String, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}");
    let coord = NodeCoord {
        store,
        spec: NodeSpec { id: id.to_string(), url: url.clone(), capacity },
    };
    let app = voice_server::router(Some(coord));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Let registration land before anyone routes against it.
    tokio::time::sleep(Duration::from_millis(80)).await;
    (url, addr)
}

async fn connect(addr: SocketAddr, room: &str, id: u32) -> Client {
    let (socket, _) = connect_async(&format!("ws://{addr}/ws?room={room}&id={id}"))
        .await
        .expect("connect");
    socket
}

fn packet(sender: u32, seq: u32) -> Vec<u8> {
    Packet {
        sender,
        seq,
        sample_rate: 48_000,
        channels: 1,
        pts_micros: seq as u64 * 10_000,
        samples: vec![10, -10, 20, -20],
    }
    .encode()
}

async fn recv_binary(client: &mut Client, ms: u64) -> Option<Vec<u8>> {
    match tokio::time::timeout(Duration::from_millis(ms), client.next()).await {
        Ok(Some(Ok(Message::Binary(b)))) => Some(b),
        _ => None,
    }
}

async fn recv_text(client: &mut Client, ms: u64) -> Option<String> {
    match tokio::time::timeout(Duration::from_millis(ms), client.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => Some(t),
        _ => None,
    }
}

fn store() -> Arc<dyn RoomStore> {
    Arc::new(MemoryStore::with_ttls(Ttls::default()))
}

#[tokio::test]
async fn three_participants_share_one_room_and_all_hear_each_other() {
    // The headline requirement: a 3-way room, with every participant receiving
    // both peers and never their own audio.
    let store = store();
    let (_, addr) = spawn_node(store.clone(), "solo", 0).await;

    let mut a = connect(addr, "trio", 1).await;
    let mut b = connect(addr, "trio", 2).await;
    let mut c = connect(addr, "trio", 3).await;
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(store.room("trio").await.unwrap().unwrap().participants, 3);

    a.send(Message::Binary(packet(1, 1))).await.unwrap();
    // Both peers hear A...
    for (name, peer) in [("b", &mut b), ("c", &mut c)] {
        let got = recv_binary(peer, 1000).await.unwrap_or_else(|| panic!("{name} heard nothing"));
        assert_eq!(Packet::decode(&got).unwrap().sender, 1);
    }
    // ...and A hears itself from neither.
    assert!(recv_binary(&mut a, 300).await.is_none());

    // And it is genuinely N-way, not just one broadcaster.
    b.send(Message::Binary(packet(2, 1))).await.unwrap();
    assert_eq!(Packet::decode(&recv_binary(&mut a, 1000).await.unwrap()).unwrap().sender, 2);
    assert_eq!(Packet::decode(&recv_binary(&mut c, 1000).await.unwrap()).unwrap().sender, 2);
}

#[tokio::test]
async fn a_room_is_released_when_the_last_participant_leaves() {
    // The lifecycle the whole coordination layer exists to get right.
    let store = store();
    let (_, addr) = spawn_node(store.clone(), "solo", 0).await;

    let a = connect(addr, "temp", 1).await;
    let b = connect(addr, "temp", 2).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(store.room("temp").await.unwrap().unwrap().participants, 2);

    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        store.room("temp").await.unwrap().unwrap().participants,
        1,
        "one leaving must not close a room others are still in"
    );

    drop(b);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        store.room("temp").await.unwrap().is_none(),
        "the room must be released once empty"
    );
}

#[tokio::test]
async fn a_client_sent_to_the_wrong_node_is_redirected_to_the_owner() {
    // Directors only hint at placement. When the hint is wrong the node must
    // hand the client the real owner rather than silently serve a split room.
    let store = store();
    let (url_a, addr_a) = spawn_node(store.clone(), "a", 0).await;
    let (_url_b, addr_b) = spawn_node(store.clone(), "b", 0).await;

    // Open the room on A.
    let _incumbent = connect(addr_a, "pinned", 1).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(store.room("pinned").await.unwrap().unwrap().node_id, "a");

    // A client arrives at B by mistake.
    let mut misrouted = connect(addr_b, "pinned", 2).await;
    let notice = recv_text(&mut misrouted, 1000).await.expect("expected a redirect");
    assert!(notice.starts_with("redirect:"), "got {notice}");
    assert!(notice.ends_with(&url_a), "should point at the owner: {notice}");

    // Critically, the bounced connection must not leave a phantom participant
    // behind — its increment has to be undone.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        store.room("pinned").await.unwrap().unwrap().participants,
        1,
        "a redirected client must not be counted"
    );
}

#[tokio::test]
async fn concurrent_arrivals_at_different_nodes_converge_on_one_room() {
    // The cold-room race, end to end through real sockets: everyone must end up
    // on a single node, with the others redirected there.
    let store = store();
    let (_, addr_a) = spawn_node(store.clone(), "a", 0).await;
    let (_, addr_b) = spawn_node(store.clone(), "b", 0).await;
    let (_, addr_c) = spawn_node(store.clone(), "c", 0).await;

    let mut tasks = Vec::new();
    for (i, addr) in [addr_a, addr_b, addr_c, addr_a, addr_b, addr_c].into_iter().enumerate() {
        tasks.push(tokio::spawn(async move {
            let mut sock = connect(addr, "cold", i as u32 + 1).await;
            // A redirect arrives as the first text frame; silence means admitted.
            recv_text(&mut sock, 600).await
        }));
    }
    let mut redirects = 0;
    let mut admitted = 0;
    for t in tasks {
        match t.await.unwrap() {
            Some(t) if t.starts_with("redirect:") => redirects += 1,
            _ => admitted += 1,
        }
    }

    let room = store.room("cold").await.unwrap().expect("room exists");
    assert!(admitted >= 1, "at least one client must have been admitted");
    assert_eq!(
        room.participants, admitted,
        "the store must count exactly the admitted clients (redirects: {redirects})"
    );
}

#[tokio::test]
async fn draining_a_node_stops_new_rooms_without_disturbing_live_calls() {
    // Scaling in. The live call must survive; only new placement changes.
    let store = store();
    // Only "old" exists when the call starts, so the room is deterministically
    // placed there — the real sequence when you scale out in order to retire a
    // node. (Connecting to a node does NOT place the room there: the store
    // decides, and a mismatch is answered with a redirect.)
    let (_, addr_old) = spawn_node(store.clone(), "old", 0).await;

    let mut incumbent = connect(addr_old, "ongoing", 1).await;
    let mut peer = connect(addr_old, "ongoing", 2).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(store.room("ongoing").await.unwrap().unwrap().node_id, "old");

    // Scale out, then retire the old node.
    let (url_new, addr_new) = spawn_node(store.clone(), "new", 0).await;
    store.set_draining("old", true).await.unwrap();

    // The existing call keeps flowing.
    incumbent.send(Message::Binary(packet(1, 5))).await.unwrap();
    assert!(
        recv_binary(&mut peer, 1000).await.is_some(),
        "draining must not interrupt an in-progress call"
    );

    // A brand-new room avoids the draining node: a client that still dials the
    // old one is bounced to the replacement.
    let mut misrouted = connect(addr_old, "brand-new", 9).await;
    let notice = recv_text(&mut misrouted, 1000).await.expect("expected a redirect");
    assert_eq!(
        notice,
        format!("redirect:{url_new}"),
        "a draining node must send new rooms to a healthy one"
    );

    // The bounced attempt leaves nothing behind — its increment was undone, so
    // the room it briefly created is released rather than orphaned on a node
    // nobody is connected to.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        store.room("brand-new").await.unwrap().is_none(),
        "a room nobody actually joined must not linger"
    );

    // Following the redirect, as a real client does, lands the room on "new".
    let _followed = connect(addr_new, "brand-new", 9).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(store.room("brand-new").await.unwrap().unwrap().node_id, "new");
}

#[tokio::test]
async fn a_node_reports_its_true_load_and_the_fleet_reflects_it() {
    // The autoscaler's input. The node's own count is authoritative, so this is
    // what corrects any drift in the incremental coordination counters.
    let store = store();
    let (_, addr) = spawn_node(store.clone(), "counted", 0).await;

    let _a = connect(addr, "r1", 1).await;
    let _b = connect(addr, "r1", 2).await;
    let _c = connect(addr, "r2", 3).await;
    // Heartbeats run on a 5s period; wait for one to land.
    tokio::time::sleep(Duration::from_millis(5_400)).await;

    let nodes = store.list_nodes().await.unwrap();
    let node = nodes.iter().find(|n| n.spec.id == "counted").expect("node listed");
    assert_eq!(node.load.participants, 3);
    assert_eq!(node.load.rooms, 2);
}
