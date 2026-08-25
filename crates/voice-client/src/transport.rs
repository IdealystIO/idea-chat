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
//! # Two ways a dial is not worth waiting for
//!
//! A QUIC handshake that nothing answers does not fail fast: the browser retries
//! until its own timeout, on the order of ten seconds. On the join path, with
//! somebody watching, that is not an acceptable way to discover a fallback.
//!
//! So this gives up twice. **Before dialling**, if the node published an address
//! this page could not reach even in principle — see
//! [`datagram::unreachable_from`], which is the tunnelled case and the common
//! one. **While dialling**, after [`DIAL_TIMEOUT_MS`], which covers a blocked
//! UDP port and anything else that simply goes quiet.

use futures::FutureExt;
use serde::Deserialize;

/// How long to wait for a QUIC session before giving up on it.
///
/// A handshake to a reachable node settles in well under a tenth of this even
/// on a poor mobile link; anything slower is a node that is not going to
/// answer. The browser's own timeout is around ten seconds, which is a fine
/// number for a page load and a terrible one for a join.
const DIAL_TIMEOUT_MS: i32 = 3_000;

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

    // A QUIC session cannot be reverse-proxied, so a node behind a gateway
    // publishes its OWN address — which, for a tunnelled deployment, is a
    // loopback address this page has no route to. Dialling it anyway would
    // stall the join for the full handshake timeout to reach a conclusion
    // available for free right here.
    if let Some(why) = datagram::unreachable_from(&page_host(), &endpoint.url) {
        return Choice::WebSocket(why);
    }

    // The same query the WebSocket would carry, from the same builder — the
    // node parses both with one parser. Carrying the ticket matters more here
    // than it looks: a node that checks tickets refuses a session without one,
    // and the browser reports that refusal as "Opening handshake failed", the
    // same words it uses for a blocked port. Dialling unauthenticated would
    // report every ticketed node as unreachable.
    let url = format!("{}?{}", endpoint.url, join.query());
    let dial = datagram::Link::connect(&url, Some(hash)).fuse();
    let deadline = sleep(DIAL_TIMEOUT_MS).fuse();
    futures::pin_mut!(dial, deadline);
    futures::select! {
        dialled = dial => match dialled {
            Ok(link) => Choice::WebTransport(link),
            Err(e) => Choice::WebSocket(format!("{} did not open: {e}", endpoint.url)),
        },
        // Dropping the dial does not cancel the handshake underneath — it is a
        // JS promise and it will run itself out — so a session may still open
        // and be discarded a moment from now. That costs a few seconds of an
        // orphaned connection and saves seven seconds on every join that was
        // never going to work, which is the right way round.
        _ = deadline => Choice::WebSocket(format!(
            "{} did not answer within {DIAL_TIMEOUT_MS} ms",
            endpoint.url,
        )),
    }
}

/// `setTimeout`, as a future.
async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// Where this page is served from, as `host` or `host:port`.
fn page_host() -> String {
    web_sys::window().and_then(|w| w.location().host().ok()).unwrap_or_default()
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
