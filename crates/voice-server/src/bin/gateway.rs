//! The standalone gateway binary — one public origin for the whole system.
//!
//! The routing itself lives in [`voice_server::gateway`] so the chat server can
//! mount the same `/_voice/*` routes on its own origin. This binary is the
//! deployment where voice is the only thing being served.

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

    axum::serve(listener, voice_server::gateway::router(director, app)).await.expect("gateway failed");
}
