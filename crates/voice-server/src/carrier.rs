//! What a participant's connection has to be able to do, so the relay does not
//! have to know which one it is.
//!
//! Two transports reach this node and they are not interchangeable in kind. A
//! WebSocket is one reliable ordered byte stream and everything — audio,
//! notices, close — shares it. A WebTransport session is a datagram channel
//! *and* a set of reliable streams, and the whole reason for using it is that
//! audio takes the unreliable half.
//!
//! So the split here is not "send bytes / send text". It is **what may be
//! dropped**:
//!
//! - [`Downlink::binary`] carries audio. Late audio is worthless, so this is
//!   the half that becomes a datagram and is allowed to disappear.
//! - [`Downlink::text`] carries notices — a redirect, a duplicate id, what
//!   suppression the room is getting. Losing one of those is not a glitch, it
//!   is a client that spends the call wrong. This half stays reliable on both
//!   transports.
//!
//! A WebSocket answers both with the same socket, which is exactly the
//! head-of-line blocking we are moving away from — but it stays, because UDP is
//! not universally reachable and a call that connects beats a call that is
//! theoretically better.

use std::future::Future;

/// One thing arriving from a participant.
#[derive(Debug)]
pub enum Incoming {
    /// A voice packet.
    Binary(Vec<u8>),
    /// A notice. Reserved for signalling, which does not live here yet.
    Text(String),
    /// The participant is gone. Terminal — a carrier that has said this once
    /// must keep saying it rather than blocking forever, because the relay's
    /// two directions race and the loser is polled again on the way out.
    Closed,
}

/// The relay -> participant direction.
///
/// Every method answers `false`/returns rather than erroring: at this layer
/// there is exactly one thing to do about a failure, which is to stop, and the
/// select in `relay` already does that.
pub trait Downlink: Send + 'static {
    /// Send audio. `false` means the connection is gone.
    ///
    /// A payload the transport cannot carry is NOT a connection failure — a
    /// datagram has a size limit and a packet over it is one bad packet. Such a
    /// send reports success so the call continues, and the carrier counts it.
    fn binary(&mut self, bytes: Vec<u8>) -> impl Future<Output = bool> + Send;

    /// Send a notice, reliably. `false` means the connection is gone.
    fn text(&mut self, text: String) -> impl Future<Output = bool> + Send;

    /// Close politely. Best effort by definition — the far side may already be
    /// gone, which is usually why we are closing.
    fn finish(&mut self) -> impl Future<Output = ()> + Send;
}

/// The participant -> relay direction.
pub trait Uplink: Send + 'static {
    fn next(&mut self) -> impl Future<Output = Incoming> + Send;
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};

impl Downlink for SplitSink<WebSocket, Message> {
    async fn binary(&mut self, bytes: Vec<u8>) -> bool {
        self.send(Message::Binary(bytes)).await.is_ok()
    }

    async fn text(&mut self, text: String) -> bool {
        self.send(Message::Text(text)).await.is_ok()
    }

    async fn finish(&mut self) {
        let _ = self.close().await;
    }
}

impl Uplink for SplitStream<WebSocket> {
    async fn next(&mut self) -> Incoming {
        loop {
            return match StreamExt::next(self).await {
                Some(Ok(Message::Binary(b))) => Incoming::Binary(b),
                Some(Ok(Message::Text(t))) => Incoming::Text(t),
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => Incoming::Closed,
                // Ping and Pong are the runtime's business. Swallowed here
                // rather than surfaced as an empty notice, which the relay
                // would then have to know to ignore.
                Some(Ok(_)) => continue,
            };
        }
    }
}

// ---------------------------------------------------------------------------
// WebTransport
// ---------------------------------------------------------------------------

/// Split a QUIC link into the two halves the relay drives independently.
///
/// One `Link`, cloned: the session is shared and each half only ever touches
/// its own direction, which is what makes the relay's two tasks possible
/// without a lock between them.
pub fn split(link: datagram::Link, id: u32) -> (DatagramDown, DatagramUp) {
    (DatagramDown { link: link.clone(), id, oversize: 0 }, DatagramUp { link })
}

/// The relay -> participant direction over QUIC.
pub struct DatagramDown {
    link: datagram::Link,
    /// For the log line, so an operator can tell which participant is at fault.
    id: u32,
    /// Packets refused for being larger than a datagram.
    ///
    /// Counted rather than fatal, and reported once. A packet over the limit is
    /// a sender that has not encoded its audio — one bad *stream*, not one bad
    /// connection — and killing the session would take down a call that is
    /// otherwise working for everyone else in the room.
    oversize: u32,
}

impl Downlink for DatagramDown {
    async fn binary(&mut self, bytes: Vec<u8>) -> bool {
        match self.link.send(&bytes).await {
            Ok(()) => true,
            Err(datagram::LinkError::TooLarge { size, limit }) => {
                self.oversize += 1;
                if self.oversize == 1 {
                    eprintln!(
                        "[oversize] id={} {size} bytes over a {limit}-byte datagram — \
                         that sender is not encoding its audio; dropping these",
                        self.id,
                    );
                }
                // Not a connection failure. The call goes on without this one.
                true
            }
            Err(_) => false,
        }
    }

    async fn text(&mut self, text: String) -> bool {
        self.link.control(&text).await.is_ok()
    }

    async fn finish(&mut self) {
        // 0 is "no error": the relay is closing because the participant left or
        // was redirected, both of which are ordinary.
        self.link.close(0, "bye");
    }
}

/// The participant -> relay direction over QUIC.
pub struct DatagramUp {
    link: datagram::Link,
}

impl Uplink for DatagramUp {
    async fn next(&mut self) -> Incoming {
        match self.link.recv().await {
            datagram::Event::Data(bytes) => Incoming::Binary(bytes),
            datagram::Event::Control(text) => Incoming::Text(text),
            datagram::Event::Closed => Incoming::Closed,
        }
    }
}
