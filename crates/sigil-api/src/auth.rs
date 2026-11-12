//! Signing a device in, by answering a challenge with its signature key.
//!
//! ```text
//! challenge() ──▶ nonce
//! device.sign(sigil_server::auth::LABEL, payload(server name, nonce))
//! sign_in(device key, nonce, signature) ──▶ Session (a cookie on the web, a token on native)
//! ```

use server::server;
use server_kit::Auth;

use crate::{Answer, Principal, Session};

#[server(path = "sigil/challenge", tags(limit = "30/min"))]
pub async fn challenge() -> Answer<String> {
    Ok(crate::srv::ops::challenge())
}

/// Answer a challenge. `device` is the signature key in hex; `nonce` and
/// `signature` are base64url.
#[server(path = "sigil/sign_in", tags(limit = "30/min"))]
pub async fn sign_in(device: String, nonce: String, signature: String) -> Answer<Session> {
    use crate::srv::{app, fail, ops};
    use crate::{hex, ApiError};
    let id = ops::answer(&device, &nonce, &signature)?;
    let principal = Principal { device: hex::encode(&id) };
    let ticket = app().sessions.start(&principal).await.map_err(|e| {
        eprintln!("[sigil] session store: {e:?}");
        fail(ApiError::Unavailable { message: "Couldn't sign in right now.".into() })
    })?;
    Ok(Session { token: ticket.token, device: principal.device })
}

#[server(path = "sigil/sign_out")]
pub async fn sign_out(me: Auth<Principal>) -> Answer<()> {
    let _ = me;
    let _ = crate::srv::app().sessions.end().await;
    Ok(())
}

/// A ticket for one live connection, good for a minute.
#[server(path = "sigil/live_ticket", tags(limit = "60/min"))]
pub async fn live_ticket(me: Auth<Principal>) -> Answer<String> {
    use crate::srv::{app, device_of, mint_ticket, now_ms};
    Ok(mint_ticket(&app().live_secret, &device_of(&me.0)?, now_ms()))
}
