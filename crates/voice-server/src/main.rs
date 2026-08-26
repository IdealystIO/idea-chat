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
//! | `VOICE_WEBTRANSPORT` | UDP port for the QUIC listener. Unset means WebSocket only |
//! | `VOICE_PUBLIC_HOST` | hostname clients dial for QUIC. Defaults to `PUBLIC_URL`'s host |
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

    // Say which mode this is, every time. An open relay is a legitimate
    // configuration for a demo and a serious mistake for a deployment, and the
    // difference is one unset variable — so it does not get to be quiet.
    match voice_server::ticket_secret() {
        Some(_) => println!("[node] room tickets REQUIRED (VOICE_SHARED_SECRET is set)"),
        None => {
            println!("[node] VOICE_SHARED_SECRET unset — this node admits ANYONE");
            println!("       who knows a room name. Correct for the loopback demo;");
            println!("       set it to the chat server's secret for a real deployment.");
        }
    }

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
    // Said out loud for the same reason the ticket mode is: this decides
    // whether the node spends its own CPU on behalf of whoever connects, and a
    // capacity decision that is only visible in an environment variable is one
    // nobody checks until the machine is already saturated.
    if voice_server::suppress::allowed() {
        println!("[node] server-side noise suppression PERMITTED ({}=on)", voice_server::suppress::ENV_ALLOW);
        println!("       ~0.11 ms of CPU per 10 ms hop per talking participant.");
    } else {
        println!("[node] server-side noise suppression off — clients asking for it");
        println!("       are told so and get an ordinary relay. Set {}=on to allow it.", voice_server::suppress::ENV_ALLOW);
    }

    println!("voice-node {id} listening on ws://{addr}/ws?room=<name>&id=<n>");

    let (app, state) = voice_server::node(coord.clone());

    // WebTransport, if this node is asked for it. Opt-in rather than always on
    // because it is a second socket on a UDP port a deployment has to open, and
    // a node that silently listened on one would be a surprise in a firewall
    // audit. Clients that cannot reach it fall back to the WebSocket, which is
    // why this is additive and not a switch.
    match std::env::var("VOICE_WEBTRANSPORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        Some(quic_port) => {
            // The certificate has to name what clients DIAL, which behind NAT
            // is not what this process binds.
            //
            // Usually that is `PUBLIC_URL`'s host, which already carries the
            // distinction for the WebSocket. But the two genuinely come apart
            // when a gateway is in front: the WebSocket is PROXIED, so
            // `PUBLIC_URL` has to be an address the gateway can reach —
            // `127.0.0.1` on a single box — while QUIC cannot be proxied at
            // all and has to name an address the BROWSER can reach. Publishing
            // a loopback address to the internet is not a subtle failure, but
            // it is a quiet one: the client refuses to dial it and every call
            // runs on the WebSocket with nothing in the UI to say so.
            let host = std::env::var("VOICE_PUBLIC_HOST").ok().filter(|h| !h.is_empty())
                .unwrap_or_else(|| {
                    public_url
                        .trim_start_matches("ws://")
                        .trim_start_matches("wss://")
                        .split(':')
                        .next()
                        .unwrap_or("127.0.0.1")
                        .to_string()
                });
            let names: Vec<&str> = if host == "127.0.0.1" || host == "localhost" {
                vec!["127.0.0.1", "localhost"]
            } else {
                vec![host.as_str(), "127.0.0.1", "localhost"]
            };
            let bind: std::net::SocketAddr = format!("0.0.0.0:{quic_port}")
                .parse()
                .expect("a port number makes an address");
            match voice_server::webtransport::listen(
                bind,
                &names,
                &host,
                state.clone(),
                state.webtransport_slot(),
            ) {
                Ok(endpoint) => {
                    println!("[node] WebTransport on {}", endpoint.url);
                    println!("       certificate hash {}", endpoint.cert_hash);
                    println!("       published at http://{addr}/webtransport");
                    // Browsers cap a pinned self-signed certificate at about
                    // two weeks, and this one is minted at startup. Said out
                    // loud because the failure mode is a node that has been up
                    // for a fortnight and stops admitting new callers.
                    println!("       self-signed and minted just now — browsers stop trusting");
                    println!("       a pinned certificate after ~14 days, so this node needs a");
                    println!("       restart or a rotation before then.");
                }
                Err(e) => {
                    // Not fatal: the WebSocket is up and calls work over it.
                    eprintln!("[node] WebTransport could not start on :{quic_port}: {e}");
                    eprintln!("       relaying over WebSocket only.");
                }
            }
        }
        None => println!("[node] WebSocket only — set VOICE_WEBTRANSPORT=<udp port> to add QUIC"),
    }

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
