//! The voice **node**: a room relay that participates in a coordinated fleet.
//!
//! A node forwards every binary frame a participant sends to every other
//! participant in the same room. It decodes nothing and mixes nothing — it
//! relays and drops.
//!
//! # Coordination
//!
//! The node owns both sides of the participant count, bound to the socket
//! lifecycle: `join` on connect, `leave` on disconnect. See [`coord`] for why
//! the director deliberately does not do the counting.
//!
//! Running without a store (`coord: None`) is a supported single-node mode —
//! useful for development and small deployments, and what the original step-1
//! relay was.
//!
//! # Why a plain binary WebSocket
//!
//! The framework's typed `Socket<In, Out>` frames are JSON text on send, which
//! is the wrong shape for PCM. This is the seam that becomes QUIC datagrams
//! later; the relay's fan-out is unaffected by that change.

pub mod coord;
pub mod suppress;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub mod gateway;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use voice_coord::{Epoch, NodeLoad};

use crate::coord::{Admission, NodeCoord, REDIRECT};

/// How many packets may back up for one slow receiver before it loses the
/// oldest. At ~10 ms per chunk this is roughly two seconds of audio.
///
/// Lagging is survivable by design: a stalled receiver should drop old audio and
/// resume at the live edge, because stale voice is worth less than current
/// voice. `RecvError::Lagged` is logged and skipped, never fatal.
const ROOM_BACKLOG: usize = 256;

/// Milliseconds of audio in one packet leaving this node.
///
/// Only the suppression path re-packetises — a plain relay forwards whatever
/// the sender cut. It matches the client's own `PACKET_MS` because a room
/// where the two disagreed would have one talker arriving in a different
/// rhythm from everyone else.
const PACKET_MS: u32 = 20;

/// Text frame sent to a participant whose id is already in the room.
///
/// A duplicate id is this relay's worst failure mode: the no-self-echo rule
/// suppresses every packet between the two, so both sides sit at zero peers in
/// silence with no error anywhere. It looks exactly like a broken network.
pub const DUPLICATE_ID: &str = "error:duplicate-id";

/// Text frame sent when no node could take the room.
pub const NO_CAPACITY: &str = "error:no-capacity";

/// One relayed packet: sender id (to skip echoing it back) plus untouched wire
/// bytes. `Arc` so fan-out to N receivers copies a pointer, not the payload.
type Relayed = (u32, Arc<Vec<u8>>);

struct RoomState {
    tx: broadcast::Sender<Relayed>,
    /// Id -> how many live sockets are claiming it.
    ///
    /// A COUNT, not a set, and the difference is the whole reason this type
    /// changed. A refresh means one id is briefly claimed twice: the browser
    /// opens the new socket before the node has finished reaping the old one.
    /// With a set, the stale socket's departure removed the id the *live* one
    /// depends on — and when that emptied the set it deleted the room out from
    /// under everybody still in it. See [`Rooms::detach`].
    ids: HashMap<u32, u32>,
    /// Assignment generation this room is being served under.
    epoch: Epoch,
}

#[derive(Clone, Default)]
struct Rooms {
    inner: Arc<Mutex<HashMap<String, RoomState>>>,
}

impl Rooms {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RoomState>> {
        self.inner.lock().expect("rooms mutex poisoned")
    }

    /// Attach to `room` at `epoch`, returning the channel pair and whether this
    /// id collided with one already present.
    ///
    /// If `epoch` is newer than the generation being served locally, the old
    /// generation is **superseded**: its sender is dropped, which ends every
    /// outbound task and closes those sockets. That is the fencing step — it
    /// stops a node that was partitioned away from continuing to serve a room
    /// that has since been reassigned to it afresh.
    fn attach(
        &self,
        room: &str,
        id: u32,
        epoch: Epoch,
    ) -> (broadcast::Sender<Relayed>, broadcast::Receiver<Relayed>, bool) {
        let mut map = self.lock();
        let state = map.entry(room.to_string()).or_insert_with(|| RoomState {
            tx: broadcast::channel(ROOM_BACKLOG).0,
            ids: HashMap::new(),
            epoch,
        });
        if epoch > state.epoch {
            eprintln!(
                "[fence] room={room} superseded: epoch {} -> {epoch}; closing stale sockets",
                state.epoch
            );
            *state = RoomState {
                tx: broadcast::channel(ROOM_BACKLOG).0,
                ids: HashMap::new(),
                epoch,
            };
        }
        let claims = state.ids.entry(id).or_insert(0);
        *claims += 1;
        let duplicate = *claims > 1;
        (state.tx.clone(), state.tx.subscribe(), duplicate)
    }

    /// Release one socket's claim on `id`, dropping the room only once no
    /// socket is attached to it at all.
    ///
    /// The count is what makes that safe. A refresh puts two sockets on one id
    /// for as long as it takes the node to notice the first one is gone, and
    /// when this removed by id instead of by claim, the stale socket's
    /// departure deleted the room while the live one was still in it. The next
    /// person to join then built a **fresh** room under the same name, with a
    /// fresh broadcast channel — so the room silently split in two and the two
    /// halves could not hear each other. It presents as one person refreshing
    /// and coming back to a call where everybody else has gone quiet, while
    /// their own microphone still shows as sending.
    fn detach(&self, room: &str, id: u32) {
        let mut map = self.lock();
        if let Some(state) = map.get_mut(room) {
            if let Some(claims) = state.ids.get_mut(&id) {
                *claims -= 1;
                if *claims == 0 {
                    state.ids.remove(&id);
                }
            }
            if state.ids.is_empty() {
                map.remove(room);
            }
        }
    }

    /// Live load, reported on every heartbeat. The node's own view is
    /// authoritative — it is the only party that knows who is really connected,
    /// so this self-corrects any drift in the coordination counters.
    fn load(&self) -> NodeLoad {
        let map = self.lock();
        NodeLoad {
            rooms: map.len() as u32,
            // Sockets, not ids: a refresh in progress really is two, and the
            // capacity this feeds is about sockets.
            participants: map.values().map(|r| r.ids.values().sum::<u32>()).sum(),
        }
    }
}

/// Shared node state.
#[derive(Clone)]
pub struct NodeState {
    rooms: Rooms,
    coord: Option<NodeCoord>,
    /// The node's one suppression host. Present whether or not suppression is
    /// permitted — see [`suppress::Suppression::start`].
    suppress: suppress::Suppression,
}

#[derive(serde::Deserialize)]
struct JoinParams {
    room: Option<String>,
    id: Option<u32>,
    /// A `voice_auth` ticket, when the deployment requires one.
    token: Option<String>,
    /// What conditioning the client is asking for. A REQUEST, not a setting:
    /// only `server` asks anything of this node, and the node answers on the
    /// socket rather than silently.
    suppress: Option<String>,
}

/// The shared secret tickets are signed with, if this node requires them.
///
/// Absent, the node admits anyone who knows a room name — which is what the
/// two-tab loopback demo needs and what a chat deployment must never run. The
/// binaries say loudly which mode they came up in; see `main`.
pub fn ticket_secret() -> Option<Vec<u8>> {
    match std::env::var("VOICE_SHARED_SECRET") {
        Ok(s) if !s.is_empty() => Some(s.into_bytes()),
        _ => None,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<JoinParams>,
    State(state): State<NodeState>,
) -> Response {
    let room = params.room.unwrap_or_else(|| "lobby".to_string());
    let id = params.id.unwrap_or(0);
    let asked = params.suppress.as_deref().map(voice_dsp::Mode::parse).unwrap_or_default();

    // Checked before the upgrade, so a refusal is an HTTP status the client can
    // read rather than a socket that opens and then dies for no stated reason.
    if let Some(secret) = ticket_secret() {
        let token = params.token.unwrap_or_default();
        match voice_auth::verify(&secret, &room, &token, now_secs()) {
            Ok(t) => {
                println!("[admit] room={room} id={id} user={}", t.user);
            }
            Err(e) => {
                eprintln!("[refuse] room={room} id={id}: {e}");
                return (StatusCode::FORBIDDEN, "a valid room ticket is required").into_response();
            }
        }
    }

    ws.on_upgrade(move |socket| relay(socket, state, room, id, asked)).into_response()
}

/// Ask the store whether this node may serve `room`.
async fn admit(coord: &Option<NodeCoord>, room: &str) -> Admission {
    let Some(c) = coord else {
        // Single-node mode: no fleet, so this node owns every room.
        return Admission::Admitted { epoch: 0, participants: 0 };
    };
    match c.store.join(room).await {
        Ok(a) if a.node_id == c.spec.id => {
            Admission::Admitted { epoch: a.epoch, participants: a.participants }
        }
        Ok(a) => {
            // The director's placement hint lost a race for a cold room. Undo
            // the increment we just made against the winner, or this connection
            // leaks a participant that nothing will ever remove.
            if let Err(e) = c.store.leave(room).await {
                eprintln!("[coord] could not undo a redirected join: {e}");
            }
            Admission::Redirect { url: a.node_url }
        }
        Err(e) => Admission::Rejected { reason: e.to_string() },
    }
}

/// Drive one participant: pump their inbound frames into the room, and the
/// room's frames (minus their own) back out.
async fn relay(socket: WebSocket, state: NodeState, room: String, id: u32, asked: voice_dsp::Mode) {
    let (mut sink, mut stream) = socket.split();

    let (epoch, participants) = match admit(&state.coord, &room).await {
        Admission::Admitted { epoch, participants } => (epoch, participants),
        Admission::Redirect { url } => {
            println!("[redirect] room={room} id={id} -> {url}");
            let _ = sink.send(Message::Text(format!("{REDIRECT}{url}"))).await;
            let _ = sink.close().await;
            return;
        }
        Admission::Rejected { reason } => {
            eprintln!("[reject] room={room} id={id}: {reason}");
            let _ = sink.send(Message::Text(NO_CAPACITY.to_string())).await;
            let _ = sink.close().await;
            return;
        }
    };

    let (tx, mut rx, duplicate) = state.rooms.attach(&room, id, epoch);
    println!("[join] room={room} id={id} epoch={epoch} participants={participants}");

    if duplicate {
        // Say so before any audio flows, so the client reports a real cause
        // instead of presenting as an unexplained silent channel.
        eprintln!("[dup] room={room} id={id} — already connected; both sides hear silence");
        let _ = sink.send(Message::Text(DUPLICATE_ID.to_string())).await;
    }

    // What the client asked for, and what this node will actually do. The two
    // are separate values on purpose: `Server` is the only mode that spends
    // this machine's CPU, so it is the only one that can be refused, and a
    // refusal presented as a grant is unanswerable from the far side.
    let granted = match asked {
        voice_dsp::Mode::Server if suppress::allowed() => voice_dsp::Mode::Server,
        voice_dsp::Mode::Server => {
            eprintln!(
                "[suppress] room={room} id={id} asked for server-side; \
                 refused ({}=on is not set)",
                suppress::ENV_ALLOW,
            );
            voice_dsp::Mode::Off
        }
        // Nothing for this node to do. `Client` is echoed rather than
        // normalised to `Off` because the client IS suppressing — the frame
        // reports what the room hears, not what this process did.
        other => other,
    };

    let feed = if granted == voice_dsp::Mode::Server {
        let out = tx.clone();
        let mut seq = 0u32;
        // 48 kHz mono is not a guess: it is DeepFilterNet's only output
        // format, so the packets leaving here always carry it regardless of
        // what the sender's microphone was running at.
        //
        // The model hands back whatever block size its framing produced, which
        // is not a packet size, so what leaves here is re-cut and re-encoded
        // exactly as a client would do it. Emitting the model's raw blocks
        // instead would put packets on the wire that no datagram could carry,
        // and would undo the codec for everyone in the room.
        let mut cutter = voice_proto::Packetiser::new(PACKET_MS, 48_000, 1);
        let mut encoder = voice_proto::codec::Encoded::open(48_000, 1, cutter.frame());
        let sink = Box::new(move |block: &[f32], pts: u64| {
            cutter.push(block, pts);
            cutter.drain(|samples, pts| {
                let packet = encoder.packet(id, seq, 48_000, 1, pts, samples);
                seq = seq.wrapping_add(1);
                // Fails only when the room has no other listeners, which is the
                // ordinary state of a room of one.
                let _ = out.send((id, Arc::new(packet.encode())));
            });
        });
        match state.suppress.open(voice_dsp::Tuning::default(), sink).await {
            Ok(feed) => {
                println!("[suppress] room={room} id={id} server-side suppression up");
                Some(feed)
            }
            Err(e) => {
                // The call is still a call. Relaying raw is a worse call, not
                // a failed one, so this degrades rather than disconnects.
                eprintln!("[suppress] room={room} id={id} could not start: {e} — relaying raw");
                None
            }
        }
    } else {
        None
    };

    // Say what the room is getting, always — including "nothing". A client
    // that asked for suppression and quietly did not get it would spend the
    // call wondering whether the model simply is not very good.
    let effective = if feed.is_some() { granted } else if granted == voice_dsp::Mode::Server {
        voice_dsp::Mode::Off
    } else {
        granted
    };
    let _ = sink
        .send(Message::Text(format!("{}{}", suppress::INFO_PREFIX, effective.as_str())))
        .await;

    // Room -> this participant.
    let room_out = room.clone();
    let mut outbound = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok((from, bytes)) => {
                    // Never echo a participant's own audio back at them; that is
                    // a guaranteed feedback loop the moment a speaker is on.
                    if from == id {
                        continue;
                    }
                    if sink.send(Message::Binary(bytes.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[lag] room={room_out} id={id} dropped {n} packets");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // This participant -> room.
    //
    // One decoder for this socket, because a socket carries one talker and an
    // Opus decoder carries that talker's signal model. It is built lazily: a
    // relay that is not suppressing never opens a packet and never needs one.
    let mut decoder = voice_proto::codec::Decoded::default();
    let mut inbound = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(bytes) => {
                    // Validate the header before fanning out, so one malformed
                    // client cannot spray garbage at everyone else.
                    match voice_proto::Packet::decode(&bytes) {
                        Ok(pkt) => match feed.as_ref() {
                            // Suppressed: the raw packet goes no further. What
                            // the room hears is what comes back out of the
                            // model, so forwarding this one too would put every
                            // talker in the room twice.
                            //
                            // This is the only place a relay opens a packet, so
                            // it is the only place that needs a decoder.
                            Some(feed) => match decoder.pcm(&pkt) {
                                Some(pcm) => feed.push(pkt.sample_rate, pkt.channels, &pcm),
                                None => eprintln!(
                                    "[drop] id={id} cannot decode {} at {} Hz x{}",
                                    pkt.encoding.as_str(),
                                    pkt.sample_rate,
                                    pkt.channels,
                                ),
                            },
                            // Relayed untouched — a guard, not a transform.
                            // `send` fails only with no receivers, which is
                            // normal for a room of one.
                            None => {
                                let _ = tx.send((pkt.sender, Arc::new(bytes)));
                            }
                        },
                        Err(e) => eprintln!("[drop] id={id} malformed packet: {e}"),
                    }
                }
                Message::Close(_) => break,
                // Text frames are reserved for signalling, which does not live
                // on this socket yet. Ignore rather than disconnect.
                _ => {}
            }
        }
    });

    // Either direction ending means the participant is gone.
    tokio::select! {
        _ = &mut outbound => inbound.abort(),
        _ = &mut inbound => outbound.abort(),
    }

    state.rooms.detach(&room, id);
    if let Some(c) = &state.coord {
        // The decrement paired with the increment in `admit`. Bound to the same
        // socket, so a crashed client cannot leak a participant.
        match c.store.leave(&room).await {
            Ok(voice_coord::LeaveOutcome::Closed { .. }) => {
                println!("[closed] room={room} — last participant left, assignment released");
            }
            Ok(_) => {}
            Err(e) => eprintln!("[coord] leave failed for room={room}: {e}"),
        }
    }
    println!("[leave] room={room} id={id}");
}

/// Build the node's router. `coord` is `None` for single-node mode.
pub fn router(coord: Option<NodeCoord>) -> Router {
    let rooms = Rooms::default();

    if let Some(c) = &coord {
        let reporter = rooms.clone();
        let c = c.clone();
        tokio::spawn(async move {
            c.start(move || reporter.load()).await;
        });
    }

    Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        // The web client is served from a different origin by `idealyst dev`,
        // so the relay has to be reachable cross-origin.
        .layer(CorsLayer::permissive())
        .with_state(NodeState { rooms, coord, suppress: suppress::Suppression::start() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: Epoch = 1;

    /// The bug this file's `detach` was rewritten for.
    ///
    /// The order below is not contrived — it is what a browser test does
    /// between two runs, and what two people rejoining a room after a blip
    /// do. Everybody reconnects before the node has finished noticing the old
    /// sockets are gone, so every id is claimed twice; then the stale claims
    /// drain, and under a set-of-ids the LAST of them emptied the set and
    /// deleted the room while both live sockets were still in it.
    ///
    /// Nothing looked wrong at that moment — the two who were already there
    /// still shared their channel and could still hear each other. The damage
    /// landed on the next person to attach, who found no room, built a fresh
    /// one under the same name with a fresh channel, and was inaudible in both
    /// directions. It presents as one person refreshing and coming back to a
    /// call where everyone has gone silent, while their own microphone still
    /// reports as sending.
    #[test]
    fn draining_stale_sockets_does_not_delete_a_room_people_are_in() {
        let rooms = Rooms::default();
        // The previous connections, not yet reaped.
        let (_ana_stale, _r1, _) = rooms.attach("porch", 1, EPOCH);
        let (_theo_stale, _r2, _) = rooms.attach("porch", 2, EPOCH);
        // Both reconnect before either departure is noticed.
        let (ana, _r3, dup) = rooms.attach("porch", 1, EPOCH);
        assert!(dup, "a second socket on one id is still worth reporting");
        let (_theo, _r4, _) = rooms.attach("porch", 2, EPOCH);
        // Now the stale pair drains.
        rooms.detach("porch", 1);
        rooms.detach("porch", 2);

        // Somebody else arrives. They must land in the room the other two are
        // already in — proved by sending, because two RoomStates with the same
        // name look identical and route nowhere near each other.
        let (_ren, mut ren_rx, _) = rooms.attach("porch", 3, EPOCH);
        ana.send((1, Arc::new(vec![0xAB]))).expect("ana still has listeners");
        assert_eq!(
            ren_rx.try_recv().expect("the newcomer is in the same room").0,
            1,
        );
    }

    /// The room still has to go away when everyone really has left, or a node
    /// accumulates a channel per room it has ever served.
    #[test]
    fn the_room_goes_when_the_last_socket_does() {
        let rooms = Rooms::default();
        let (_a, _arx, _) = rooms.attach("porch", 1, EPOCH);
        let (_b, _brx, _) = rooms.attach("porch", 2, EPOCH);
        rooms.detach("porch", 1);
        assert_eq!(rooms.load().rooms, 1);
        rooms.detach("porch", 2);
        assert_eq!(rooms.load().rooms, 0, "an empty room must not be kept");
    }

    /// Load is reported in sockets, because that is what the machine is
    /// actually carrying — a refresh in progress genuinely is two.
    #[test]
    fn load_counts_sockets() {
        let rooms = Rooms::default();
        let (_a, _arx, _) = rooms.attach("porch", 1, EPOCH);
        let (_b, _brx, _) = rooms.attach("porch", 1, EPOCH);
        assert_eq!(rooms.load().participants, 2);
        rooms.detach("porch", 1);
        assert_eq!(rooms.load().participants, 1);
    }

    /// Detaching an id that was never attached must not underflow the count —
    /// a `u32` going below zero would panic in debug and wrap in release,
    /// which would make the room immortal.
    #[test]
    fn an_unknown_detach_is_ignored() {
        let rooms = Rooms::default();
        let (_a, _arx, _) = rooms.attach("porch", 1, EPOCH);
        rooms.detach("porch", 99);
        rooms.detach("nowhere", 1);
        assert_eq!(rooms.load().participants, 1);
    }
}
