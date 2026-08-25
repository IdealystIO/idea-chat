//! The WebTransport listener, beside the WebSocket one.
//!
//! Same relay, different carrier — see [`carrier`](crate::carrier). What is
//! specific to this transport is everything *before* the call starts:
//!
//! - **It is not an HTTP route.** WebTransport is QUIC, which is UDP, so this
//!   is its own socket on its own port. It cannot be mounted on the axum router
//!   the way `/ws` is, and a deployment has to open a UDP port for it.
//! - **It needs a certificate**, because QUIC has no unencrypted mode. A
//!   self-hosted instance rarely has a CA one, so this generates a self-signed
//!   certificate and publishes its hash for clients to pin. See [`Identity`]
//!   for what browsers demand in return, which is rotation.
//! - **The join parameters arrive in the connect URL**, the same place the
//!   WebSocket carried them, and are checked before the session opens — so a
//!   refusal is a status the client can read rather than a connection that
//!   opens and then dies for no stated reason.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use datagram::{Identity, Listener};

use crate::{carrier, NodeState};

/// Where clients dial for WebTransport, and what they must pin to trust it.
///
/// Published over HTTP because a client cannot discover either: the port is not
/// the node's HTTP port, and the certificate is not one any CA vouches for.
#[derive(Clone, serde::Serialize)]
pub struct Endpoint {
    /// The `https://` URL to dial. WebTransport speaks https even though
    /// nothing about it is HTTP after the handshake.
    pub url: String,
    /// SHA-256 of the certificate, hex — what a browser puts in
    /// `serverCertificateHashes`.
    pub cert_hash: String,
}

/// Set once the listener is up, read by the HTTP route that publishes it.
pub type Published = Arc<OnceLock<Endpoint>>;

/// Bind a WebTransport listener and start accepting.
///
/// `names` are the hostnames and addresses the certificate must cover — what
/// clients will actually dial, which behind NAT is not what this process binds.
///
/// Returns as soon as the socket is bound; the accept loop runs in the
/// background. Binding to port 0 is supported and is why the bound address is
/// reported back rather than assumed.
pub fn listen(
    addr: SocketAddr,
    names: &[&str],
    public_host: &str,
    state: NodeState,
    published: Published,
) -> Result<Endpoint, String> {
    let identity = Identity::self_signed(names).map_err(|e| e.to_string())?;
    let cert_hash = identity.hash_hex();
    let mut listener = Listener::bind(addr, &identity).map_err(|e| e.to_string())?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;

    let endpoint = Endpoint {
        url: format!("https://{public_host}:{}/ws", bound.port()),
        cert_hash,
    };
    // Ignored if something already published: two listeners on one node is a
    // configuration error, not a race, and the first one wins rather than the
    // route flapping between them.
    let _ = published.set(endpoint.clone());

    tokio::spawn(async move {
        while let Some(request) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move { admit(request, state).await });
        }
        eprintln!("[wt] listener closed");
    });

    Ok(endpoint)
}

/// Check one connect request and, if it passes, run the relay over it.
async fn admit(request: datagram::Request, state: NodeState) {
    let url = request.url();
    let from = request.remote_addr();
    let params = Params::parse(&url);

    // Checked BEFORE the session opens, so a refusal is a status the client can
    // read. This mirrors `ws_handler`, deliberately: two transports that
    // disagreed about who may join would be a hole in whichever one was
    // checked less carefully.
    if let Some(secret) = crate::ticket_secret() {
        let token = params.token.as_deref().unwrap_or_default();
        match voice_auth::verify(&secret, &params.room, token, crate::now_secs()) {
            Ok(t) => println!("[wt admit] room={} id={} user={}", params.room, params.id, t.user),
            Err(e) => {
                eprintln!("[wt refuse] room={} id={} from={from}: {e}", params.room, params.id);
                request.refuse(403).await;
                return;
            }
        }
    }

    let link = match request.accept().await {
        Ok(link) => link,
        Err(e) => {
            eprintln!("[wt] session from {from} did not open: {e}");
            return;
        }
    };

    let (down, up) = carrier::split(link, params.id);
    crate::serve(down, up, state, params.room, params.id, params.suppress).await;
}

/// What the connect URL says about who is joining and where.
struct Params {
    room: String,
    id: u32,
    token: Option<String>,
    suppress: voice_dsp::Mode,
}

impl Params {
    /// Read the query string.
    ///
    /// Tolerant on purpose — a missing room is the lobby and a missing id is
    /// zero, matching `JoinParams`. What is NOT tolerated is a bad ticket, and
    /// that is checked separately: a parse that silently defaulted the room
    /// would hand someone a ticket-checked join to a room they did not name.
    fn parse(url: &str) -> Params {
        let mut room = None;
        let mut id = None;
        let mut token = None;
        let mut suppress = None;

        if let Some(query) = url.split_once('?').map(|(_, q)| q) {
            for pair in query.split('&') {
                let Some((key, value)) = pair.split_once('=') else { continue };
                let value = decode(value);
                match key {
                    "room" => room = Some(value),
                    "id" => id = value.parse().ok(),
                    "token" => token = Some(value),
                    "suppress" => suppress = Some(voice_dsp::Mode::parse(&value)),
                    _ => {}
                }
            }
        }

        Params {
            room: room.unwrap_or_else(|| "lobby".to_string()),
            id: id.unwrap_or(0),
            token,
            suppress: suppress.unwrap_or_default(),
        }
    }
}

/// Percent-decoding, enough for a query value.
///
/// Hand-rolled rather than pulled in: the only values here are a room name, a
/// number and a signed token, and a dependency whose whole job is `%20` is not
/// worth the supply chain. `+` is a space because form encoding says so and
/// browsers still emit it.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // Not an escape after all. A literal `%` in a room name is
                    // odd but it is not a reason to lose the character.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_join_url_yields_its_parameters() {
        let p = Params::parse("https://10.0.0.4:8798/ws?room=porch&id=42&suppress=client&token=abc");
        assert_eq!(p.room, "porch");
        assert_eq!(p.id, 42);
        assert_eq!(p.token.as_deref(), Some("abc"));
        assert_eq!(p.suppress, voice_dsp::Mode::Client);
    }

    #[test]
    fn a_parameter_this_node_does_not_know_is_ignored() {
        // Forward compatibility, and it earned its test: `probe=1` was a real
        // parameter here until the client stopped probing and started dialling
        // the session it would actually use. A client still sending it must
        // join rather than be refused.
        let p = Params::parse("https://h/ws?room=porch&id=1&probe=1");
        assert_eq!(p.room, "porch");
        assert_eq!(p.id, 1);
    }

    #[test]
    fn a_url_with_no_query_is_the_lobby_with_no_ticket() {
        let p = Params::parse("https://10.0.0.4:8798/ws");
        assert_eq!(p.room, "lobby");
        assert_eq!(p.id, 0);
        assert!(p.token.is_none(), "and no ticket, so a ticketed node refuses it");
    }

    #[test]
    fn an_escaped_room_name_comes_back_whole() {
        // Room names are user-facing text, so they contain spaces.
        let p = Params::parse("https://h/ws?room=the%20porch&id=1");
        assert_eq!(p.room, "the porch");
        let plus = Params::parse("https://h/ws?room=the+porch&id=1");
        assert_eq!(plus.room, "the porch");
    }

    #[test]
    fn a_token_survives_its_own_punctuation() {
        // Signed tickets carry base64, which means `=` padding — and `=` is the
        // separator. Splitting on the FIRST one is what keeps this whole.
        let p = Params::parse("https://h/ws?id=1&token=aGVsbG8=");
        assert_eq!(p.token.as_deref(), Some("aGVsbG8="));
    }

    #[test]
    fn a_malformed_pair_is_skipped_rather_than_derailing_the_rest() {
        let p = Params::parse("https://h/ws?room=porch&garbage&id=9");
        assert_eq!(p.room, "porch");
        assert_eq!(p.id, 9, "the pair after the bad one is still read");
    }

    #[test]
    fn an_unparseable_id_does_not_become_someone_elses() {
        // Falling back to a *different* participant's id would put two people
        // on one sender number, which is the relay's worst failure mode.
        let p = Params::parse("https://h/ws?room=porch&id=notanumber");
        assert_eq!(p.id, 0, "zero, which the relay treats as its own claim");
    }
}
