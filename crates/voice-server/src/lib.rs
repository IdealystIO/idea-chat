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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
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
    /// Ids currently connected, so a collision can be reported rather than
    /// silently swallowed by the self-echo filter.
    ids: HashSet<u32>,
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
            ids: HashSet::new(),
            epoch,
        });
        if epoch > state.epoch {
            eprintln!(
                "[fence] room={room} superseded: epoch {} -> {epoch}; closing stale sockets",
                state.epoch
            );
            *state = RoomState {
                tx: broadcast::channel(ROOM_BACKLOG).0,
                ids: HashSet::new(),
                epoch,
            };
        }
        let duplicate = !state.ids.insert(id);
        (state.tx.clone(), state.tx.subscribe(), duplicate)
    }

    /// Release `id`, dropping the room when it empties so a reconnect with the
    /// same id is not reported as a collision against its own stale entry.
    fn detach(&self, room: &str, id: u32) {
        let mut map = self.lock();
        if let Some(state) = map.get_mut(room) {
            state.ids.remove(&id);
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
            participants: map.values().map(|r| r.ids.len() as u32).sum(),
        }
    }
}

/// Shared node state.
#[derive(Clone)]
pub struct NodeState {
    rooms: Rooms,
    coord: Option<NodeCoord>,
}

#[derive(serde::Deserialize)]
struct JoinParams {
    room: Option<String>,
    id: Option<u32>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<JoinParams>,
    State(state): State<NodeState>,
) -> impl IntoResponse {
    let room = params.room.unwrap_or_else(|| "lobby".to_string());
    let id = params.id.unwrap_or(0);
    ws.on_upgrade(move |socket| relay(socket, state, room, id))
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
async fn relay(socket: WebSocket, state: NodeState, room: String, id: u32) {
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
    let mut inbound = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(bytes) => {
                    // Validate the header before fanning out, so one malformed
                    // client cannot spray garbage at everyone else. The payload
                    // is relayed untouched — this is a guard, not a transform.
                    match voice_proto::Packet::decode(&bytes) {
                        Ok(pkt) => {
                            // `send` fails only with no receivers, which is
                            // normal for a room of one.
                            let _ = tx.send((pkt.sender, Arc::new(bytes)));
                        }
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
        .with_state(NodeState { rooms, coord })
}
