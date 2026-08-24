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

    let project_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let web_dir = project_dir.join("dist/web");
    if !web_dir.exists() {
        eprintln!("hearth: {} does not exist — run `idealyst build --web` first.", web_dir.display());
    }

    // Voice, on this origin. A browser on an https page may only open wss, and
    // the session cookie is scoped to this host — pointing people at a second
    // origin for audio means a second certificate and a second set of rules to
    // get wrong. The relay fleet still runs as its own processes; this is only
    // the door.
    let voice: Option<Router> = match std::env::var("VOICE_DIRECTOR_URL") {
        Ok(url) if !url.is_empty() => {
            if srv::voice_secret().is_none() {
                eprintln!("hearth: VOICE_DIRECTOR_URL is set but VOICE_SHARED_SECRET is not.");
                eprintln!("        Voice stays OFF: without the secret this server cannot mint");
                eprintln!("        the tickets the relays check, and a relay that is not");
                eprintln!("        checking them admits anyone who can guess a channel id.");
                None
            } else {
                println!("hearth: voice enabled — relays via {url}");
                Some(voice_server::gateway::voice_routes(url))
            }
        }
        _ => None,
    };

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

    let mut app: Router = server::router()
        // A route, not another layer of `ServeDir` fallback. Chaining them
        // through `not_found_service` serves the right bytes with the OUTER
        // service's 404 still attached — a response that is correct and
        // unusable at the same time, and invisible unless you look at the
        // status rather than the body.
        .nest_service("/assets", ServeDir::new(&assets_dir))
        .fallback_service(
            ServeDir::new(&web_dir)
                .append_index_html_on_directories(true)
                .not_found_service(ServeDir::new(&web_dir)),
        );

    if let Some(voice) = voice {
        app = app.merge(voice);
    }

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
