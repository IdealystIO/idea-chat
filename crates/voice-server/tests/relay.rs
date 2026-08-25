//! Integration tests for the relay, against a real server on an ephemeral port.
//!
//! These cover the properties a voice relay has to get right and that are
//! painful to debug once audio is involved: that a participant never hears
//! themselves, that rooms are isolated, and that a malformed packet is dropped
//! rather than sprayed at everyone.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use voice_proto::Packet;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Serve the real router on port 0 and return the bound address.
async fn spawn_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, voice_server::router(None)).await.unwrap();
    });
    addr
}

async fn join(addr: SocketAddr, room: &str, id: u32) -> Client {
    let url = format!("ws://{addr}/ws?room={room}&id={id}");
    let (socket, _) = connect_async(&url).await.expect("connect");
    socket
}

fn packet(sender: u32, seq: u32) -> Vec<u8> {
    // Raw PCM, deliberately: a relay forwards bytes without opening them, and
    // these tests are about the forwarding. The one place encoding matters is
    // the suppression path, which has its own coverage.
    Packet::from_f32(
        sender,
        seq,
        48_000,
        1,
        seq as u64 * 10_000,
        &[0.00305185, -0.00305185, 0.0061037, -0.0061037],
    )
    .encode()
}

/// Read one binary frame, or `None` if nothing arrives within `ms`.
///
/// A timeout is the assertion mechanism for the negative cases (no self-echo,
/// no cross-room leakage): there is no event that says "nothing will come," so
/// waiting a bounded time and finding silence is the test.
///
/// Text frames are skipped rather than treated as silence. Every participant is
/// told what suppression the room is getting the moment they join, so that
/// notice is the FIRST thing on the socket — and a helper that answered `None`
/// to it reported "nothing arrived" for every one of these tests. They were red
/// from the day that notice was added and nothing was running them: `check-all`
/// ran `--lib` only.
async fn recv_binary(client: &mut Client, ms: u64) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.checked_duration_since(tokio::time::Instant::now())?;
        match tokio::time::timeout(left, client.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => return Some(b),
            Ok(Some(Ok(Message::Text(_)))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn relays_packet_to_the_other_participant_verbatim() {
    let addr = spawn_server().await;
    let mut alice = join(addr, "room-a", 1).await;
    let mut bob = join(addr, "room-a", 2).await;
    // Both subscriptions must exist before the send, or broadcast drops it.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let sent = packet(1, 7);
    alice.send(Message::Binary(sent.clone())).await.unwrap();

    let got = recv_binary(&mut bob, 1000).await.expect("bob receives alice's packet");
    assert_eq!(got, sent, "relay must not alter the payload");

    let decoded = Packet::decode(&got).expect("still decodes after the round trip");
    assert_eq!(decoded.sender, 1);
    assert_eq!(decoded.seq, 7);
    assert_eq!(decoded.sample_rate, 48_000);
}

#[tokio::test]
async fn never_echoes_a_participant_back_to_themselves() {
    // The bug this guards: a self-echo is an instant feedback loop the moment
    // anyone is on a speaker, and it presents as "the app screams," not as a
    // routing error.
    let addr = spawn_server().await;
    let mut alice = join(addr, "room-echo", 1).await;
    let mut bob = join(addr, "room-echo", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    alice.send(Message::Binary(packet(1, 0))).await.unwrap();

    assert!(
        recv_binary(&mut alice, 400).await.is_none(),
        "alice must never receive her own audio"
    );
    assert!(
        recv_binary(&mut bob, 400).await.is_some(),
        "but bob still should — proving the send actually happened"
    );
}

#[tokio::test]
async fn rooms_are_isolated() {
    let addr = spawn_server().await;
    let mut alice = join(addr, "room-one", 1).await;
    let mut outsider = join(addr, "room-two", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    alice.send(Message::Binary(packet(1, 0))).await.unwrap();

    assert!(
        recv_binary(&mut outsider, 400).await.is_none(),
        "a different room must not receive the audio"
    );
}

#[tokio::test]
async fn malformed_packets_are_dropped_not_relayed() {
    // The relay validates headers so one broken client cannot spray garbage at
    // everyone else's decoder.
    let addr = spawn_server().await;
    let mut alice = join(addr, "room-bad", 1).await;
    let mut bob = join(addr, "room-bad", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    alice
        .send(Message::Binary(vec![0x00, 0x01, 0x02, 0x03]))
        .await
        .unwrap();
    assert!(
        recv_binary(&mut bob, 400).await.is_none(),
        "garbage must not reach another participant"
    );

    // …and the connection survives it, so one bad frame is not a disconnect.
    alice.send(Message::Binary(packet(1, 1))).await.unwrap();
    assert!(
        recv_binary(&mut bob, 1000).await.is_some(),
        "the socket must still work after a malformed frame"
    );
}

#[tokio::test]
async fn fans_out_to_every_other_participant() {
    let addr = spawn_server().await;
    let mut alice = join(addr, "room-many", 1).await;
    let mut bob = join(addr, "room-many", 2).await;
    let mut carol = join(addr, "room-many", 3).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    alice.send(Message::Binary(packet(1, 5))).await.unwrap();

    for (name, client) in [("bob", &mut bob), ("carol", &mut carol)] {
        let got = recv_binary(client, 1000).await.unwrap_or_else(|| panic!("{name} got nothing"));
        assert_eq!(Packet::decode(&got).unwrap().seq, 5);
    }
}

#[tokio::test]
async fn text_frames_are_ignored_rather_than_fatal() {
    // Text is reserved for signalling, which does not live on this socket yet.
    // Receiving one must not tear down the media path.
    let addr = spawn_server().await;
    let mut alice = join(addr, "room-text", 1).await;
    let mut bob = join(addr, "room-text", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    alice.send(Message::Text("hello".into())).await.unwrap();
    assert!(recv_binary(&mut bob, 300).await.is_none());

    alice.send(Message::Binary(packet(1, 2))).await.unwrap();
    assert!(
        recv_binary(&mut bob, 1000).await.is_some(),
        "audio must still flow after a text frame"
    );
}

#[tokio::test]
async fn duplicate_id_in_a_room_is_reported_not_silently_swallowed() {
    // The regression this locks down: two clients claiming the same id land in
    // one room, the no-self-echo rule suppresses every packet between them, and
    // both sit at zero peers in total silence with nothing logged. It presents
    // as a dead network. The relay must say so instead.
    let addr = spawn_server().await;
    let mut first = join(addr, "room-dup", 1).await;
    let mut second = join(addr, "room-dup", 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The newcomer is told; the incumbent is undisturbed.
    let notice = tokio::time::timeout(Duration::from_millis(800), second.next()).await;
    match notice {
        Ok(Some(Ok(Message::Text(t)))) => {
            assert!(t.contains("duplicate-id"), "unexpected notice: {t}");
        }
        other => panic!("second joiner was not warned: {other:?}"),
    }
    assert!(
        recv_binary(&mut first, 300).await.is_none(),
        "the incumbent should not be sent a notice"
    );
}

/// Collect every notice a client is sent within `ms`.
///
/// The negative duplicate-id tests are "no WARNING arrives", not "nothing
/// arrives" — every participant is told what suppression the room is getting
/// the moment they join, so a test that waited for silence was asserting
/// against a notice that is supposed to be there.
async fn notices(client: &mut Client, ms: u64) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    let mut out = Vec::new();
    while let Some(left) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        match tokio::time::timeout(left, client.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => out.push(t),
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    out
}

#[tokio::test]
async fn a_distinct_id_in_the_same_room_is_not_flagged() {
    let addr = spawn_server().await;
    let _first = join(addr, "room-ok", 1).await;
    let mut second = join(addr, "room-ok", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let said = notices(&mut second, 500).await;
    assert!(
        !said.iter().any(|n| n.contains("duplicate-id")),
        "a distinct id must not be warned: {said:?}"
    );
}

#[tokio::test]
async fn reconnecting_with_the_same_id_after_leaving_is_not_a_duplicate() {
    // Ids are released on disconnect, so a page refresh does not collide with
    // its own stale entry — which would make the app unusable after one reload.
    let addr = spawn_server().await;
    let first = join(addr, "room-recycle", 9).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut again = join(addr, "room-recycle", 9).await;
    let said = notices(&mut again, 500).await;
    assert!(
        !said.iter().any(|n| n.contains("duplicate-id")),
        "reconnect with a freed id must not be flagged: {said:?}"
    );
}
