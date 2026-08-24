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

    let app: Router = server::router().fallback_service(
        ServeDir::new(&web_dir)
            .append_index_html_on_directories(true)
            .not_found_service(ServeDir::new(&web_dir)),
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
