//! The **director**: the control plane clients ask "where does this room live?"
//!
//! # What it is not
//!
//! It is not the source of truth, and it does not count participants. It answers
//! a placement question and nothing else, which is what lets you run as many
//! directors as you like behind a load balancer with no coordination between
//! them — they are stateless, and the store settles every disagreement.
//!
//! Concretely: if two directors send two clients to different nodes for the same
//! cold room, whichever node's atomic `join` lands first owns it, and the other
//! node redirects its client. The wrong answer is self-correcting, so the
//! director is free to be approximate and fast.
//!
//! # Endpoints
//!
//! - `GET  /rooms/{room}/route` — where to connect. Returns the current owner if
//!   the room is open, else the least-loaded eligible node.
//! - `GET  /rooms/{room}` — room state, or 404.
//! - `GET  /fleet` — every live node, for operators and autoscaling signals.
//! - `POST /nodes/{id}/drain` and `/undrain` — take a node out of rotation
//!   without dropping its live calls.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use voice_coord::{RoomStore, Ttls};

#[derive(Serialize)]
struct Route {
    room: String,
    node_id: String,
    ws_url: String,
    /// True when the room is already open and this is its real owner. False
    /// means "nobody is in it yet, here is where you would probably land" —
    /// a hint the node's atomic join may override with a redirect.
    authoritative: bool,
}

#[derive(Serialize)]
struct FleetView {
    nodes: Vec<NodeRow>,
    /// Total participants across the fleet — the number an autoscaler watches.
    participants: u32,
    /// Nodes accepting new rooms. Zero means new rooms are being refused and
    /// the fleet must scale out.
    accepting: usize,
}

#[derive(Serialize)]
struct NodeRow {
    id: String,
    url: String,
    rooms: u32,
    participants: u32,
    capacity: u32,
    draining: bool,
    last_seen_ms_ago: u64,
}

type Store = Arc<dyn RoomStore>;

async fn route(
    Path(room): Path<String>,
    State(store): State<Store>,
) -> Result<Json<Route>, (StatusCode, String)> {
    // An open room has exactly one right answer.
    if let Some(view) = store.room(&room).await.map_err(server_error)? {
        let nodes = store.list_nodes().await.map_err(server_error)?;
        if let Some(node) = nodes.iter().find(|n| n.spec.id == view.node_id) {
            return Ok(Json(Route {
                room,
                node_id: node.spec.id.clone(),
                ws_url: node.spec.url.clone(),
                authoritative: true,
            }));
        }
        // The owner is no longer live. Fall through and hint at a healthy node;
        // the winning node's `join` will reassign the room properly.
    }

    let nodes = store.list_nodes().await.map_err(server_error)?;
    let best = nodes
        .iter()
        .filter(|n| !n.draining)
        .filter(|n| n.spec.capacity == 0 || n.load.participants < n.spec.capacity)
        .min_by_key(|n| n.load.participants);

    match best {
        Some(n) => Ok(Json(Route {
            room,
            node_id: n.spec.id.clone(),
            ws_url: n.spec.url.clone(),
            authoritative: false,
        })),
        // 503 rather than 500: this is the "scale out" signal and it is
        // retryable, which a 5xx-generic response would not convey.
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no voice node has capacity".to_string(),
        )),
    }
}

async fn room_state(
    Path(room): Path<String>,
    State(store): State<Store>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match store.room(&room).await.map_err(server_error)? {
        Some(v) => Ok(Json(serde_json::json!({
            "room": v.room,
            "node_id": v.node_id,
            "epoch": v.epoch,
            "participants": v.participants,
        }))),
        None => Err((StatusCode::NOT_FOUND, format!("room {room} is not active"))),
    }
}

async fn fleet(State(store): State<Store>) -> Result<Json<FleetView>, (StatusCode, String)> {
    let nodes = store.list_nodes().await.map_err(server_error)?;
    let participants = nodes.iter().map(|n| n.load.participants).sum();
    let accepting = nodes
        .iter()
        .filter(|n| !n.draining)
        .filter(|n| n.spec.capacity == 0 || n.load.participants < n.spec.capacity)
        .count();
    Ok(Json(FleetView {
        participants,
        accepting,
        nodes: nodes
            .into_iter()
            .map(|n| NodeRow {
                id: n.spec.id,
                url: n.spec.url,
                capacity: n.spec.capacity,
                rooms: n.load.rooms,
                participants: n.load.participants,
                draining: n.draining,
                last_seen_ms_ago: n.last_seen_ms_ago,
            })
            .collect(),
    }))
}

async fn drain(
    Path(id): Path<String>,
    State(store): State<Store>,
) -> Result<StatusCode, (StatusCode, String)> {
    store.set_draining(&id, true).await.map_err(server_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn undrain(
    Path(id): Path<String>,
    State(store): State<Store>,
) -> Result<StatusCode, (StatusCode, String)> {
    store.set_draining(&id, false).await.map_err(server_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn server_error(e: voice_coord::StoreError) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Build the director router over any store.
pub fn router(store: Store) -> Router {
    Router::new()
        .route("/rooms/:room/route", get(route))
        .route("/rooms/:room", get(room_state))
        .route("/fleet", get(fleet))
        .route("/nodes/:id/drain", post(drain))
        .route("/nodes/:id/undrain", post(undrain))
        .route("/health", get(|| async { "ok" }))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(store)
}

/// Build the store named by `COORD_BACKEND` (`memory` | `redis` | `postgres`).
///
/// Shared with the node binary so both tiers cannot be pointed at different
/// stores by a config mistake — which would present as rooms that exist for one
/// tier and not the other.
pub async fn store_from_env() -> Arc<dyn RoomStore> {
    let ttls = Ttls::default();
    match std::env::var("COORD_BACKEND").unwrap_or_else(|_| "memory".into()).as_str() {
        "postgres" => {
            let url = std::env::var("POSTGRES_URL").expect("POSTGRES_URL required");
            let store = voice_coord::postgres_store::PostgresStore::connect(&url, ttls)
                .await
                .expect("connect postgres");
            store.migrate().await.expect("migrate");
            println!("[coord] postgres");
            Arc::new(store)
        }
        "redis" => {
            let url = std::env::var("REDIS_URL").expect("REDIS_URL required");
            let store =
                voice_coord::redis_store::RedisStore::connect(&url, "voice", ttls)
                    .await
                    .expect("connect redis");
            println!("[coord] redis");
            Arc::new(store)
        }
        other => {
            if other != "memory" {
                eprintln!("[coord] unknown COORD_BACKEND '{other}', using memory");
            }
            // Only correct for a single-process deployment: a second director
            // would have its own, disjoint view of the fleet.
            println!("[coord] memory (single process only)");
            Arc::new(voice_coord::memory::MemoryStore::with_ttls(ttls))
        }
    }
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let store = store_from_env().await;
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("voice-director on http://{addr}  (GET /rooms/<room>/route, GET /fleet)");
    axum::serve(listener, router(store)).await.expect("director failed");
}
