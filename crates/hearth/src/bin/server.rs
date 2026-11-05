//! The Hearth server: the `#[server]` API at `/_srv/*`, plus the wasm bundle.
//!
//! ```sh
//! cd crates/hearth && idealyst build --web
//! DATABASE_URL=postgres://postgres:dev@localhost:55432/hearth \
//!   cargo run -p hearth --bin server --features server
//! ```
//!
//! One origin serves both, which is not incidental: the session is an
//! httpOnly `SameSite=Lax` cookie, and a cookie set by one origin is not sent
//! to another. Splitting the API onto a second host is what turns a working
//! login into a mystery 401.

use std::path::PathBuf;

use axum::Router;
use hearth::srv;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:dev@localhost:55432/hearth".into());
    let bind = std::env::var("HEARTH_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let db = hearth_db::Db::connect(&db_url).await.expect("connect to the database");
    db.migrate().await.expect("apply migrations");

    // Sessions ride the cache contract. In-memory is correct for a single
    // process and wrong the moment there are two — every session would live on
    // exactly one instance and every other one would 401. `REDIS_URL` is what
    // makes a multi-instance deployment work, so its absence is a warning
    // rather than a silent default.
    let sessions = match std::env::var("REDIS_URL") {
        Ok(url) => {
            let client = redis_client(&url);
            server::install_state(client);
            server_kit::Sessions::new(cache::RedisCache::from_installed()).sliding()
        }
        Err(_) => {
            eprintln!("hearth: REDIS_URL unset — sessions are in-memory.");
            eprintln!("        Fine for one process; sessions are lost on restart and");
            eprintln!("        are not shared if you run a second instance.");
            server_kit::Sessions::new(cache::MemoryCache::default()).sliding()
        }
    };

    // First-run: if there are no accounts, mint a one-time claim link. Held in
    // memory, so restarting issues a fresh one and a lost link is recoverable
    // without a password reset flow that does not exist yet.
    let (bootstrap, claim) = srv::Bootstrap::arm(&db).await.expect("check for accounts");

    // The live state, loaded from the database once so a restart does not
    // forget who is in a call, then kept current by the events themselves.
    let hub: srv::LiveHub = idea_state::hub::Hub::new(srv::LiveStore::load(&db).await);

    // Presence is a lease, and this is the only thing that reaps one. Nothing
    // else removes somebody who did not say they were leaving — a socket
    // closing shortens their lease (`Db::voice_fading`) but does not end their
    // call, because the call is a different socket and is usually still up.
    //
    // Without this task an expired lease would be invisible to reads and never
    // announced, so every client already connected would keep drawing a ghost
    // until it happened to reconnect.
    srv::sweep_presence(db.clone(), hub.clone());

    server::install_state(hub);
    server::install_state(srv::AppDb(db));
    server::install_state(sessions.clone());
    server::install_state(bootstrap);
    server_kit::install_middleware(sessions.guard::<hearth::api::Principal>());

    // Defense in depth over the SameSite=Lax cookie. Set HEARTH_ORIGINS to
    // your real web origin(s) in production — a wrong list here is a rejected
    // login, which is at least loud.
    //
    // The default allows every spelling of the loopback address on the port we
    // actually bound. `localhost`, `127.0.0.1` and `[::1]` reach the same
    // socket but are three *different* origins, and the browser sends whichever
    // one is in the address bar. Allowing only the one the operator happened to
    // type into HEARTH_BIND makes the app work or 403 based on how they later
    // typed the URL, which reads as "the server is down".
    let origins: Vec<String> = std::env::var("HEARTH_ORIGINS")
        .unwrap_or_else(|_| {
            let port = bind.rsplit(':').next().unwrap_or("8080");
            format!("http://localhost:{port},http://127.0.0.1:{port},http://[::1]:{port}")
        })
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    server_kit::install_middleware(server_kit::csrf_guard(
        origins.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    ));

    // `CARGO_MANIFEST_DIR` is baked in at COMPILE time, which is right for
    // `cargo run` from a checkout and wrong for anything deployed: the binary
    // is built somewhere that does not exist on the machine serving it, so it
    // looks for the bundle in a directory that was never copied there and
    // silently serves 404s for the whole app.
    //
    // `HEARTH_ROOT` is the override. Unset, nothing changes.
    let project_dir = std::env::var("HEARTH_ROOT")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let web_dir = project_dir.join("dist/web");
    if !web_dir.exists() {
        eprintln!("hearth: {} does not exist — run `idealyst build --web` first.", web_dir.display());
    }

    // Voice: a call-relay, in this process, on its own UDP port. Clients get
    // a grant from `voice_ticket` and dial it over WebTransport; moderators'
    // removals apply to it directly through its handle. Off without the
    // secret: a relay that is not checking grants admits anyone who can guess
    // a channel id.
    start_call_relay();

    // The noise-suppression model, served from its own directory rather than
    // from `dist/web`, because `idealyst build` owns that one and an 8 MB file
    // sitting in a directory something else regenerates is a file that
    // disappears on a Tuesday. Absent is fine: client-side suppression reports
    // that it cannot start and the call goes ahead without it.
    let assets_dir = project_dir.join("assets");
    if !assets_dir.join("DeepFilterNet3_onnx.tar.gz").exists() {
        eprintln!(
            "hearth: no noise-suppression model in {} — run ./tools/fetch-model.sh.",
            assets_dir.display(),
        );
        eprintln!("        On-device suppression will be unavailable; on-server is unaffected.");
    }

    let index = std::fs::read_to_string(web_dir.join("index.html")).unwrap_or_else(|e| {
        eprintln!("hearth: could not read {}: {e}", web_dir.join("index.html").display());
        String::new()
    });

    let app: Router = server::router()
        // A route, not another layer of `ServeDir` fallback. Chaining them
        // through `not_found_service` serves the right bytes with the OUTER
        // service's 404 still attached — a response that is correct and
        // unusable at the same time, and invisible unless you look at the
        // status rather than the body.
        .nest_service("/assets", ServeDir::new(&assets_dir))
        // A single-page app, so every path the file system does not have is
        // the app's to route, not a 404. Without this `/claim/<token>` and
        // `/j/<code>` are dead links — which they were: the server printed a
        // claim URL at startup that could only be pasted, never clicked.
        //
        // `fallback`, NOT `not_found_service`. The two take the same argument
        // and read the same, and they differ in exactly one way that matters
        // here: `not_found_service` forces the status to 404 whatever the
        // inner service returned, while `fallback` passes it through.
        //
        // With the wrong one, this served the whole app under a 404 — the page
        // renders, because browsers render 404 bodies, so it looks like it
        // works while every cache and crawler is told the app does not exist.
        // Diagnosed by making the handler return a marker and finding the
        // marker in a 404 body, which is the only way to tell the two apart
        // from outside.
        //
        // The index is read once at startup: it is 800 bytes and does not
        // change while the process runs.
        .fallback_service(
            ServeDir::new(&web_dir)
                .append_index_html_on_directories(true)
                .fallback(axum::routing::get(move || {
                    let index = index.clone();
                    async move { axum::response::Html(index) }
                })),
        );

    println!("Hearth is running.");
    println!("  UI  → http://{bind}/");
    println!("  API → http://{bind}/_srv/<fn>");
    // Printed because a mismatch here is invisible from the browser: the
    // request 403s with no body, and the app reports that it cannot reach the
    // server. Seeing the list next to the URL you typed is the whole diagnosis.
    println!("  Browser origins allowed: {}", origins.join(" "));
    if let Some(token) = claim {
        println!();
        println!("This instance has no accounts yet.");
        println!("  Claim it: http://{bind}/claim/{token}");
        println!("  This link works once. Restart to issue a new one.");
    }
    println!();

    let addr: std::net::SocketAddr = bind.parse().expect("HEARTH_BIND must be host:port");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}

fn redis_client(url: &str) -> redis::Client {
    redis::Client::open(url).expect("REDIS_URL must be a valid redis:// URL")
}

/// Start the call relay, if voice is configured.
///
/// - `CALL_RELAY_BIND`: where to listen (UDP), default `0.0.0.0:8798`.
/// - `CALL_RELAY_HOST`: the name browsers dial, default `127.0.0.1` — not
///   `localhost`, which a browser may resolve to `::1` first and find nothing
///   listening (the bind below is IPv4).
/// - `CALL_RELAY_CERT` / `CALL_RELAY_KEY`: a CA-issued chain and key (PEM), for
///   production. Without them the relay makes a self-signed certificate whose
///   hash `voice_ticket` hands out for pinning — which browsers accept for
///   about two weeks, so that is for development, or a restart at least that
///   often.
fn start_call_relay() {
    let Some(secret) = srv::voice_secret() else {
        eprintln!("hearth: VOICE_SHARED_SECRET unset — voice is off.");
        return;
    };
    let bind = std::env::var("CALL_RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8798".into());
    let host = std::env::var("CALL_RELAY_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    // The CA certificate when it is there and readable; otherwise — first boot,
    // before the TLS proxy has one — a self-signed one, said out loud.
    let ca = match (std::env::var("CALL_RELAY_CERT"), std::env::var("CALL_RELAY_KEY")) {
        (Ok(cert), Ok(key)) => match (std::fs::read(&cert), std::fs::read(&key)) {
            (Ok(c), Ok(k)) => match datagram::Identity::from_pem(&c, &k) {
                Ok(identity) => Some(identity),
                Err(e) => {
                    eprintln!("hearth: {cert}: {e} — the call relay falls back to a self-signed certificate.");
                    None
                }
            },
            _ => {
                eprintln!("hearth: cannot read {cert} / {key} — the call relay falls back to a self-signed certificate.");
                None
            }
        },
        _ => None,
    };
    let pinned = ca.is_none();
    let identity = match ca {
        Some(identity) => identity,
        None => datagram::Identity::self_signed(&[host.as_str(), "localhost", "127.0.0.1"]).expect("certificate"),
    };
    let addr: std::net::SocketAddr = bind.parse().expect("CALL_RELAY_BIND must be host:port");
    let listener = match datagram::Listener::bind(addr, &identity) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hearth: could not bind the call relay on {addr}: {e} — voice is off.");
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(addr.port());
    let config = call_relay::Config {
        auth: Some(call_relay::Auth {
            secret,
            // Renewal refreshes a grant well inside this; it bounds how long a
            // revocation must be remembered.
            max_ttl: std::time::Duration::from_millis(srv::GRANT_TTL_MS * 2),
        }),
        grace: std::time::Duration::from_secs(15),
        // Holding back video from members whose download cannot take it all:
        // steadier calls for them, a header read per datagram per member for
        // the server. Off unless asked for.
        throttle: matches!(std::env::var("CALL_RELAY_THROTTLE").as_deref(), Ok("on" | "1" | "true")),
        ..Default::default()
    };
    let handle = call_relay::spawn(listener, config);
    let url = format!("https://{host}:{port}/");
    println!("hearth: voice on — call relay at {url}{}", if pinned { " (self-signed)" } else { "" });
    srv::install_call_relay(srv::CallRelay {
        handle,
        url,
        cert_hash: if pinned { identity.hash_hex() } else { String::new() },
    });
}
