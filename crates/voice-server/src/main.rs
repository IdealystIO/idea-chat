//! Binary entry for a voice node. Behavior lives in the library so integration
//! tests can serve the same router.
//!
//! # Configuration
//!
//! | env | meaning |
//! | --- | --- |
//! | `PORT` | listen port (default 8787) |
//! | `NODE_ID` | fleet identity; defaults to `node-<port>` |
//! | `PUBLIC_URL` | the `ws://` URL clients dial. **Must be reachable by clients**, not by the node — behind NAT or a load balancer these differ |
//! | `CAPACITY` | max participants before the node refuses NEW rooms (0 = unbounded) |
//! | `COORD_BACKEND` | `memory` \| `redis` \| `postgres`. Unset means single-node mode |
//!
//! Without `COORD_BACKEND` the node runs standalone and owns every room, which
//! is the original single-relay behavior and correct for development.

use std::sync::Arc;

use voice_coord::NodeSpec;
use voice_server::coord::NodeCoord;

#[path = "bin/director.rs"]
#[allow(dead_code)]
mod director_shared;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8787);
    let id = std::env::var("NODE_ID").unwrap_or_else(|_| format!("node-{port}"));
    let public_url =
        std::env::var("PUBLIC_URL").unwrap_or_else(|_| format!("ws://127.0.0.1:{port}"));
    let capacity: u32 = std::env::var("CAPACITY").ok().and_then(|c| c.parse().ok()).unwrap_or(0);

    let coord = match std::env::var("COORD_BACKEND") {
        Ok(_) => {
            let store: Arc<dyn voice_coord::RoomStore> = director_shared::store_from_env().await;
            println!("[node] {id} joining the fleet as {public_url} (capacity {capacity})");
            Some(NodeCoord {
                store,
                spec: NodeSpec { id: id.clone(), url: public_url.clone(), capacity },
            })
        }
        Err(_) => {
            println!("[node] single-node mode — no coordination store configured");
            None
        }
    };

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("voice-node {id} listening on ws://{addr}/ws?room=<name>&id=<n>");

    let app = voice_server::router(coord.clone());
    let server = axum::serve(listener, app);

    // Graceful shutdown: drain first so the fleet stops sending NEW rooms here,
    // then leave. Existing calls keep running until their sockets close, which
    // is the difference between scaling in and dropping people mid-sentence.
    tokio::select! {
        r = server => { r.expect("node failed"); }
        _ = tokio::signal::ctrl_c() => {
            if let Some(c) = &coord {
                println!("[node] draining…");
                c.drain().await;
                c.deregister().await;
            }
        }
    }
}
