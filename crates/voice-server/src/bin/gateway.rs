//! The **gateway**: one public origin for the whole system.
//!
//! # Why this exists
//!
//! A tunnel (ngrok, Cloudflare, a corporate LB) gives you exactly ONE public
//! origin. The system natively needs three: the web app, the director, and a
//! per-node `ws://` address. Worse, two of those are addresses a remote machine
//! cannot reach — `ws://127.0.0.1:8787` means "the visitor's own laptop".
//!
//! The gateway collapses all of it onto a single host and, critically,
//! **rewrites every node address on the way out** so nothing a client receives
//! ever points at an address only the server can resolve.
//!
//! ```text
//!   remote browser
//!        │  https://xxx.ngrok.app
//!        ▼
//!   ┌─────────────────────────────────────────────┐
//!   │ gateway :8080                               │
//!   │   /_voice/rooms/{room}/route ──▶ director   │  ws_url rewritten
//!   │   /_voice/node/{id}/ws ────────▶ that node  │  redirect: frames rewritten
//!   │   /*  ─────────────────────────▶ web app    │
//!   └─────────────────────────────────────────────┘
//! ```
//!
//! # The two rewrites, and why both are required
//!
//! 1. **Placement.** The director answers with a node's real `ws://host:port`.
//!    Returned verbatim, a remote client dials its own machine. The gateway
//!    replaces it with `{scheme}://{this-host}/_voice/node/{id}`.
//! 2. **Redirects.** When a node does not own a room it replies with a
//!    `redirect:ws://other-node:8787` text frame. That address has the same
//!    problem, and it is easy to miss because it only fires on the cold-room
//!    race. The proxy inspects text frames and rewrites those too — otherwise
//!    remote calls work *most* of the time and fail exactly when two people
//!    join an empty room at once.
//!
//! # Public origin is derived, never configured
//!
//! The rewritten URLs are built from the request's own `Host` and
//! `X-Forwarded-Proto`. So the same binary serves `http://localhost:8080` and
//! `https://xxx.ngrok.app` with no configuration — and `X-Forwarded-Proto:
//! https` correctly yields `wss://`, which browsers require on a secure page.
//!
//! # A secure origin is not optional
//!
//! `getUserMedia` only works on a secure context. A LAN IP over plain HTTP is
//! NOT one, so remote testing over `http://192.168.x.x` fails with a silent
//! permanently-muted mic. A tunnel's HTTPS origin is what makes remote testing
//! work at all — this is a reason to use one, not just a convenience.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{Message as AxumMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMsg;

/// How long the node-address map is reused before refetching from the director.
///
/// Short, because it is consulted to rewrite redirects: a stale entry sends a
/// client to a node that no longer owns the room, which the client would then
/// have to be redirected away from again.
const FLEET_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Gateway {
    director: String,
    app: String,
    http: reqwest::Client,
    fleet: Arc<Mutex<FleetCache>>,
}

#[derive(Default)]
struct FleetCache {
    /// node url -> node id, for rewriting redirects back into gateway paths.
    by_url: Vec<(String, String)>,
    fetched: Option<Instant>,
}

impl Gateway {
    /// node id -> its real ws URL.
    async fn node_url(&self, id: &str) -> Option<String> {
        self.fleet_map()
            .await
            .into_iter()
            .find(|(_, node_id)| node_id == id)
            .map(|(url, _)| url)
    }

    /// The node registry, refetched when stale.
    async fn fleet_map(&self) -> Vec<(String, String)> {
        let mut cache = self.fleet.lock().await;
        let fresh = cache.fetched.is_some_and(|t| t.elapsed() < FLEET_TTL);
        if fresh {
            return cache.by_url.clone();
        }
        if let Ok(resp) = self.http.get(format!("{}/fleet", self.director)).send().await {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                let nodes = v.get("nodes").and_then(|n| n.as_array()).cloned().unwrap_or_default();
                cache.by_url = nodes
                    .iter()
                    .filter_map(|n| {
                        Some((
                            n.get("url")?.as_str()?.trim_end_matches('/').to_string(),
                            n.get("id")?.as_str()?.to_string(),
                        ))
                    })
                    .collect();
                cache.fetched = Some(Instant::now());
            }
        }
        cache.by_url.clone()
    }
}

/// The origin this request arrived on, as the client sees it.
///
/// Behind a tunnel the socket-level address is meaningless — only the forwarded
/// headers describe what the browser actually typed.
fn public_origin(headers: &HeaderMap) -> (String, String) {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080")
        .to_string();
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.eq_ignore_ascii_case("https"))
        // A tunnel always terminates TLS and sets the header. Without it, assume
        // plain http — guessing https for a local run would hand the client a
        // wss:// URL nothing is listening on.
        .unwrap_or(false);
    let ws = if https { "wss" } else { "ws" };
    let http = if https { "https" } else { "http" };
    (format!("{http}://{host}"), format!("{ws}://{host}"))
}

/// The gateway path a client should dial for `node_id`.
fn node_path(ws_origin: &str, node_id: &str) -> String {
    // No trailing `/ws`: the client appends that itself.
    format!("{ws_origin}/_voice/node/{node_id}")
}

/// Placement, with the node address rewritten to a gateway path.
async fn route(
    Path(room): Path<String>,
    headers: HeaderMap,
    State(gw): State<Gateway>,
) -> Response {
    let upstream = format!("{}/rooms/{}/route", gw.director, room);
    let Ok(resp) = gw.http.get(&upstream).send().await else {
        return (StatusCode::BAD_GATEWAY, "director unreachable").into_response();
    };
    let status = resp.status();
    let Ok(mut body) = resp.json::<serde_json::Value>().await else {
        // A 503 from the director (no capacity) has no JSON body; pass the
        // status through so the client still sees the scale-out signal.
        return (status, "").into_response();
    };

    let (_, ws_origin) = public_origin(&headers);
    if let Some(id) = body.get("node_id").and_then(|v| v.as_str()).map(|s| s.to_string()) {
        body["ws_url"] = serde_json::Value::String(node_path(&ws_origin, &id));
    }
    (status, Json(body)).into_response()
}

/// Read-only director passthroughs, for operators and the harnesses.
async fn director_get(uri: Uri, State(gw): State<Gateway>) -> Response {
    let tail = uri.path().trim_start_matches("/_voice");
    let Ok(resp) = gw.http.get(format!("{}{}", gw.director, tail)).send().await else {
        return (StatusCode::BAD_GATEWAY, "director unreachable").into_response();
    };
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    (status, [(axum::http::header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

#[derive(serde::Deserialize)]
struct NodeQuery {
    room: Option<String>,
    id: Option<u32>,
}

/// WebSocket proxy to one node.
async fn node_ws(
    ws: WebSocketUpgrade,
    Path(node_id): Path<String>,
    Query(q): Query<NodeQuery>,
    headers: HeaderMap,
    State(gw): State<Gateway>,
) -> Response {
    let Some(target) = gw.node_url(&node_id).await else {
        return (StatusCode::NOT_FOUND, format!("unknown node {node_id}")).into_response();
    };
    let room = q.room.unwrap_or_else(|| "lobby".into());
    let id = q.id.unwrap_or(0);
    let upstream = format!("{}/ws?room={}&id={}", target.trim_end_matches('/'), room, id);
    let (_, ws_origin) = public_origin(&headers);

    ws.on_upgrade(move |client| pump(client, upstream, ws_origin, gw))
}

/// Splice a client socket to a node socket, rewriting node addresses in transit.
async fn pump(client: WebSocket, upstream: String, ws_origin: String, gw: Gateway) {
    let node = match tokio_tungstenite::connect_async(&upstream).await {
        Ok((s, _)) => s,
        Err(e) => {
            eprintln!("[gateway] cannot reach {upstream}: {e}");
            return;
        }
    };
    let (mut node_tx, mut node_rx) = node.split();
    let (mut client_tx, mut client_rx) = client.split();

    // client -> node
    let up = tokio::spawn(async move {
        while let Some(Ok(msg)) = client_rx.next().await {
            let out = match msg {
                AxumMsg::Binary(b) => WsMsg::Binary(b),
                AxumMsg::Text(t) => WsMsg::Text(t),
                AxumMsg::Close(_) => break,
                // Ping/pong are handled by each hop's own transport.
                _ => continue,
            };
            if node_tx.send(out).await.is_err() {
                break;
            }
        }
    });

    // node -> client, rewriting redirects
    let down = tokio::spawn(async move {
        while let Some(Ok(msg)) = node_rx.next().await {
            let out = match msg {
                WsMsg::Binary(b) => AxumMsg::Binary(b),
                WsMsg::Text(t) => {
                    // The one frame that carries an address. Left alone, it
                    // sends a remote client to a host only the server can
                    // resolve — and only on the cold-room race, so it would
                    // look like an intermittent failure rather than a bug.
                    AxumMsg::Text(match t.strip_prefix("redirect:") {
                        Some(url) => match rewrite_redirect(url, &ws_origin, &gw).await {
                            Some(rewritten) => format!("redirect:{rewritten}"),
                            None => {
                                eprintln!("[gateway] unmapped redirect target {url}");
                                t
                            }
                        },
                        None => t,
                    })
                }
                WsMsg::Close(_) => break,
                _ => continue,
            };
            if client_tx.send(out).await.is_err() {
                break;
            }
        }
    });

    // Either half ending tears down the pair.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

/// Turn a node's own ws URL into the gateway path for that node.
async fn rewrite_redirect(url: &str, ws_origin: &str, gw: &Gateway) -> Option<String> {
    let want = url.trim_end_matches('/');
    let id = gw
        .fleet_map()
        .await
        .into_iter()
        .find(|(node_url, _)| node_url == want)
        .map(|(_, id)| id)?;
    Some(node_path(ws_origin, &id))
}

/// Everything else is the web app.
async fn app_proxy(State(gw): State<Gateway>, req: Request) -> Response {
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{}{}", gw.app, path);

    let mut out = gw.http.request(req.method().clone(), &url);
    for (name, value) in req.headers() {
        // Host must describe the UPSTREAM, not the tunnel, or the dev server
        // may reject or misroute the request.
        if name != axum::http::header::HOST {
            out = out.header(name, value);
        }
    }
    let body = axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024).await.unwrap_or_default();

    let Ok(resp) = out.body(body).send().await else {
        return (
            StatusCode::BAD_GATEWAY,
            format!("web app unreachable at {}", gw.app),
        )
            .into_response();
    };

    let status = resp.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in resp.headers() {
        // Re-chunked by this hop; a copied length or encoding would disagree
        // with the body actually sent.
        if name == axum::http::header::CONTENT_LENGTH
            || name == axum::http::header::TRANSFER_ENCODING
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    builder.body(Body::from(bytes)).unwrap_or_else(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, "bad upstream response").into_response()
    })
}

/// Build the gateway router.
pub fn router(director: String, app: String) -> Router {
    let gw = Gateway {
        director,
        app,
        http: reqwest::Client::new(),
        fleet: Arc::new(Mutex::new(FleetCache::default())),
    };
    Router::new()
        .route("/_voice/rooms/:room/route", get(route))
        .route("/_voice/node/:node_id/ws", get(node_ws))
        .route("/_voice/fleet", get(director_get))
        .route("/_voice/rooms/:room", get(director_get))
        .route("/_voice/health", get(|| async { "ok" }))
        .fallback(any(app_proxy))
        .with_state(gw)
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let director =
        std::env::var("DIRECTOR_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    // Where `idealyst dev` is serving the client. Moved off 8080 so the gateway
    // can own the single public port.
    let app = std::env::var("APP_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into());

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));

    println!("voice-gateway on http://{addr}");
    println!("  director : {director}");
    println!("  web app  : {app}");
    println!();
    println!("  Point the client's first field at:  <this-origin>/_voice");
    println!("  Expose it with:  ngrok http {port}");

    axum::serve(listener, router(director, app)).await.expect("gateway failed");
}
