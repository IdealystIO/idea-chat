//! A Sigil server in one process:
//!
//! - the Sigil API (`/_srv/sigil/*`) and live connections
//!   (`/_srv/_ws/sigil_live`);
//! - the relay for lossy services (QUIC, on its own UDP port).
//!
//! Running the relay here is safe in a way it was not for the old call key
//! exchange: no key passes through this process. Lossy frames are sealed
//! under keys every member derives from the group's epoch, which the server
//! never holds.
//!
//! Everything is in memory — groups, records, sessions — so this is one
//! instance, and a restart forgets everything. Configured from the
//! environment:
//!
//! | Variable | Default |
//! | --- | --- |
//! | `SIGIL_BIND` | `127.0.0.1:8091` |
//! | `SIGIL_NAME` (the audience devices sign in to and address actions to) | `localhost` |
//! | `SIGIL_PUBLIC_URL` (allowed browser origin) | `http://localhost:8091` |
//! | `SIGIL_RELAY_BIND` | `127.0.0.1:4434` |
//! | `SIGIL_RELAY_URL` (as devices dial it) | `https://localhost:4434/` |
//! | `SIGIL_RELAY_CERT`, `SIGIL_RELAY_KEY` (PEM; else self-signed) | — |
//! | `SIGIL_RELAY_SECRET`, `SIGIL_LIVE_SECRET` (≥ 32 chars; else random per run) | — |

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sigil::group::Unverified;
use sigil_api::srv::{App, Relay, Router};
use sigil_api::Principal;
use sigil_server::{Challenges, Hub, MemoryStore};

// Referencing the API crate keeps the linker from dropping its routes.
use sigil_api::auth::challenge as _force_link;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// A secret from the environment, or a random one for this run, said out loud.
fn secret(name: &str) -> Vec<u8> {
    match env(name) {
        Some(s) if s.len() >= 32 => s.into_bytes(),
        Some(_) => panic!("{name} must be at least 32 characters"),
        None => {
            eprintln!("[sigil] {name} is not set; using a random one for this run");
            let bytes: [u8; 32] = rand::random();
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD_NO_PAD, bytes).into_bytes()
        }
    }
}

/// Start the relay. Returns how devices reach it, or `None` if it couldn't
/// bind (lossy services then won't connect; everything else works).
fn start_relay(bind: SocketAddr, url: String, secret: &[u8]) -> Option<Relay> {
    let (identity, hash) = match (env("SIGIL_RELAY_CERT"), env("SIGIL_RELAY_KEY")) {
        (Some(cert), Some(key)) => {
            let chain = std::fs::read(cert).expect("read SIGIL_RELAY_CERT");
            let key = std::fs::read(key).expect("read SIGIL_RELAY_KEY");
            (datagram::Identity::from_pem(&chain, &key).expect("SIGIL_RELAY_CERT / SIGIL_RELAY_KEY are a PEM chain and key"), String::new())
        }
        _ => {
            let id = datagram::Identity::self_signed(&["localhost", "127.0.0.1"]).expect("self-signed relay certificate");
            let hash = id.hash_hex();
            (id, hash)
        }
    };
    let listener = match datagram::Listener::bind(bind, &identity) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[sigil] the relay could not bind {bind}: {e:?}");
            return None;
        }
    };
    let handle = Arc::new(sigil_relay::spawn(listener, secret));
    println!("  relay  → {url} (udp {bind})");
    let revoke: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |token: String| {
        let handle = handle.clone();
        tokio::spawn(async move {
            if let Err(e) = handle.revoke(&token).await {
                eprintln!("[sigil] relay revocation refused: {e:?}");
            }
        });
    });
    Some(Relay { url, cert_hash: hash, issuer: sigil_relay::Issuer::new(secret), revoke })
}

/// Rate-limit by device when signed in, else by client address.
fn principal_key(ctx: &server::Context) -> Option<String> {
    ctx.get::<Principal>().map(|p| format!("device:{}", p.device))
}

#[tokio::main]
async fn main() {
    let _ = _force_link;
    let bind: SocketAddr = env("SIGIL_BIND").unwrap_or_else(|| "127.0.0.1:8091".into()).parse().expect("SIGIL_BIND is host:port");
    let name = env("SIGIL_NAME").unwrap_or_else(|| "localhost".into());
    let public_url = env("SIGIL_PUBLIC_URL").unwrap_or_else(|| format!("http://localhost:{}", bind.port()));
    let relay_bind: SocketAddr =
        env("SIGIL_RELAY_BIND").unwrap_or_else(|| "127.0.0.1:4434".into()).parse().expect("SIGIL_RELAY_BIND is host:port");
    let relay_url = env("SIGIL_RELAY_URL").unwrap_or_else(|| format!("https://localhost:{}/", relay_bind.port()));
    println!("Sigil server \"{name}\"");
    eprintln!("[sigil] credentials are not checked against an identity service yet; this is for development");

    let sessions = server_kit::Sessions::new(cache::MemoryCache::new())
        .cookie_name("sigil_session")
        .cookie({
            let secure = public_url.starts_with("https://");
            move |c| c.secure(secure)
        })
        .ttl(Duration::from_secs(60 * 60 * 24 * 30))
        .sliding();
    server_kit::install_middleware(sessions.guard::<Principal>());
    server_kit::install_middleware(server_kit::rate_limit().key_by(principal_key).store(server_kit::MemoryLimitStore::default()));
    server_kit::install_middleware(server_kit::csrf_guard([public_url.clone()]));

    let relay = start_relay(relay_bind, relay_url, &secret("SIGIL_RELAY_SECRET"));
    sigil_api::srv::install(App {
        hub: Mutex::new(Hub::new(&name, MemoryStore::default(), Unverified)),
        challenges: Mutex::new(Challenges::new(&name)),
        sessions,
        live_secret: secret("SIGIL_LIVE_SECRET"),
        relay,
        router: Router::default(),
    });

    let app = server::router();
    println!("  api    → {public_url}/_srv/sigil/*");
    let listener = tokio::net::TcpListener::bind(bind).await.expect("bind SIGIL_BIND");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.expect("serve");
}
