//! Which transport a call should use, and finding out.
//!
//! A node offers WebSocket always and WebTransport sometimes. The client cannot
//! know which without asking, and it cannot know *how* to dial WebTransport
//! without being told two things it has no way to guess: the UDP port, which is
//! not the node's HTTP port, and the certificate hash, because a self-hosted
//! instance's certificate is not one any CA vouches for.
//!
//! So the node publishes both at `GET /webtransport`, and this asks.
//!
//! # Why it dials rather than believing the answer
//!
//! A published endpoint is not a reachable one. UDP is blocked on plenty of
//! corporate networks, and a browser that cannot open a QUIC session fails at
//! connect — after the call has already been "started" as far as the person is
//! concerned. Dialling here, before any audio path exists, turns that into a
//! quiet fallback instead of a call that never begins.
//!
//! The dial is the real one: this returns the live session, and the call runs
//! on it. An earlier version probed with a throwaway session and then made the
//! call on the WebSocket, which cost an extra round trip to prove something the
//! join itself proves.
//!
//! # What it costs on a network where UDP is blocked
//!
//! A QUIC handshake that nothing answers does not fail fast — the browser
//! retries until its own timeout, on the order of ten seconds, and there is no
//! way to shorten it from here without a timer the runtime does not yet
//! provide. So joining a call from behind a UDP-blocking firewall stalls before
//! falling back. Recorded in docs/quic-migration.md rather than hidden, because
//! the fix is a race against a sleep and that is a framework gap, not a voice
//! one.

use serde::Deserialize;

/// What a node published about its WebTransport listener.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Endpoint {
    /// The `https://` URL to dial.
    pub url: String,
    /// SHA-256 of the certificate, hex.
    pub cert_hash: String,
}

/// What this client will actually use, and why.
///
/// No `Debug`, no `PartialEq`: the WebTransport arm holds a live session, which
/// is not a value to compare. What a test wants to assert on is
/// [`name`](Choice::name) and [`reason`](Choice::reason), and both are here.
pub enum Choice {
    /// QUIC datagrams, on a session that is open right now and ready to carry
    /// the call.
    WebTransport(datagram::Link),
    /// The WebSocket, with the reason WebTransport was not used. The reason is
    /// carried rather than logged so the caller decides where it goes — and so
    /// a test can assert on it, which is the difference between "the fallback
    /// works" and "the fallback is all that ever runs".
    WebSocket(String),
}

impl Choice {
    /// The one word that goes in a log line or a stats probe.
    pub fn name(&self) -> &'static str {
        match self {
            Choice::WebTransport(_) => "webtransport",
            Choice::WebSocket(_) => "websocket",
        }
    }

    /// Why the WebSocket, when it is the WebSocket.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Choice::WebTransport(_) => None,
            Choice::WebSocket(why) => Some(why),
        }
    }
}

/// Ask a node what it offers, and open it before believing the answer.
///
/// `node_url` is the `ws://` or `wss://` URL the WebSocket path would use; the
/// HTTP origin is derived from it, because that is the one address a client is
/// certain to have.
///
/// A [`Choice::WebTransport`] carries a session that has already been accepted
/// by the node — ticket checked, room joined. There is nothing left to prove
/// about it.
pub async fn choose(node_url: &str, join: &crate::carrier::Join<'_>) -> Choice {
    let base = http_origin(node_url);
    let endpoint = match publish(&base).await {
        Ok(Some(e)) => e,
        // 404 is the ordinary answer from a node relaying over WebSocket only.
        Ok(None) => return Choice::WebSocket("node offers WebSocket only".to_string()),
        Err(e) => return Choice::WebSocket(format!("could not ask {base}: {e}")),
    };

    let Some(hash) = datagram::parse_hash(&endpoint.cert_hash) else {
        return Choice::WebSocket(format!(
            "node published an unreadable certificate hash: {}",
            endpoint.cert_hash,
        ));
    };

    // The same query the WebSocket would carry, from the same builder — the
    // node parses both with one parser. Carrying the ticket matters more here
    // than it looks: a node that checks tickets refuses a session without one,
    // and the browser reports that refusal as "Opening handshake failed", the
    // same words it uses for a blocked port. Dialling unauthenticated would
    // report every ticketed node as unreachable.
    let url = format!("{}?{}", endpoint.url, join.query());
    match datagram::Link::connect(&url, Some(hash)).await {
        Ok(link) => Choice::WebTransport(link),
        Err(e) => Choice::WebSocket(format!("{} did not open: {e}", endpoint.url)),
    }
}

/// `GET /webtransport`, or `None` when the node does not offer one.
async fn publish(base: &str) -> Result<Option<Endpoint>, String> {
    let url = format!("{base}/webtransport");
    let response = net::Client::new().get(&url).send().await.map_err(|e| e.to_string())?;
    // 404 is the answer from a node relaying over WebSocket only. That is
    // information, not a failure — hence `Ok(None)`.
    if response.status() == 404 {
        return Ok(None);
    }
    // Anything else that does not parse IS a failure, and one worth naming.
    // Folding it into "offers WebSocket only" was the difference between a
    // one-word verdict and a diagnosis: a misconfigured node and an absent
    // feature looked identical from the browser.
    let status = response.status();
    match response.json::<Endpoint>().await {
        Ok(e) => Ok(Some(e)),
        Err(e) => Err(format!("{url} answered {status} with unusable JSON: {e}")),
    }
}

/// The HTTP origin a `ws://` URL implies.
///
/// A node's HTTP and WebSocket endpoints are the same server on the same port —
/// only the scheme differs — so this is a rewrite, not a guess. `wss` maps to
/// `https` because a page served over TLS cannot fetch plaintext anyway.
fn http_origin(node_url: &str) -> String {
    let trimmed = node_url.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        trimmed.to_string()
    }
}

/// These run only under a wasm test runner, which this repo does not have — the
/// module is web-only because `datagram` needs the browser's WebTransport. What
/// actually covers this path is `tools/voice-flows.js`, which asserts from a
/// real browser that the choice comes out `webtransport`. Kept because they run
/// the moment such a runner exists, and because a wrong `http_origin` is a
/// silent fallback rather than an error.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ws_node_url_becomes_its_http_origin() {
        assert_eq!(http_origin("ws://127.0.0.1:8797"), "http://127.0.0.1:8797");
        assert_eq!(http_origin("ws://127.0.0.1:8797/"), "http://127.0.0.1:8797");
        assert_eq!(http_origin("wss://voice.example.com"), "https://voice.example.com");
    }

    #[test]
    fn an_origin_that_is_already_http_is_left_alone() {
        // Hearth mounts the relay on its own origin, so this is a real case.
        assert_eq!(http_origin("https://chat.example.com"), "https://chat.example.com");
    }

    #[test]
    fn a_choice_carries_why_it_fell_back() {
        let c = Choice::WebSocket("node offers WebSocket only".to_string());
        assert_eq!(c.name(), "websocket");
        assert!(c.reason().unwrap().contains("WebSocket only"));
    }

    #[test]
    fn both_carriers_ask_to_join_in_the_same_words() {
        // The node runs one parser over both URLs. This is the assertion that
        // stops the two drifting apart — a WebTransport dial that spelled the
        // room differently would join `lobby` and be silent in `porch`, on the
        // transport nobody is watching.
        let join = crate::carrier::Join {
            room: "porch",
            id: 7,
            suppress: voice_dsp::Mode::Off,
            ticket: Some("t"),
        };
        assert_eq!(join.query(), "room=porch&id=7&suppress=off&token=t");
    }
}
