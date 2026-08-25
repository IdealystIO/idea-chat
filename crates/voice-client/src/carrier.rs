//! What a call's connection has to be able to do, so the audio path does not
//! have to know which transport it is on.
//!
//! The mirror of the relay's [`carrier`](../../voice_server/carrier/index.html)
//! module, and split the same way — by **what may be dropped**, not by "binary
//! or text". Audio is the half that is allowed to disappear; a redirect or a
//! duplicate-id notice is not.
//!
//! What is different on this side is the *shape* of the send. The capture
//! callback is a plain synchronous callback: it is handed a block of audio and
//! must return. `WsSender::send` is synchronous too, so the WebSocket path was
//! a straight call from inside it. [`datagram::Link::send`] is `async`, and
//! that mismatch is the whole reason this file exists.
//!
//! # The queue, and why it is short
//!
//! Crossing from a synchronous callback to an async send takes a queue and a
//! task to drain it. This queue holds ten packets and no more. It is there to
//! cross that boundary and for nothing else: at 20 ms a packet, a full queue
//! means the send path is already a fifth of a second behind, and audio that
//! old is not worth delivering. So it drops rather than grows — the same trade
//! the transport underneath is already making, and counted the same way, in
//! [`Stats::unsent`](crate::session::Stats::unsent).

use net::{WebSocket, WsMessage, WsSender};

/// Everything the connect URL has to say, on either carrier.
///
/// One type and one [`query`](Join::query) because the node parses both with
/// one parser. A WebSocket URL and a WebTransport URL that disagreed about a
/// parameter name would be a bug visible only on whichever transport happened
/// to be tested less — which, for most of this project's life, was the new one.
pub struct Join<'a> {
    pub room: &'a str,
    pub id: u32,
    /// Where this client is asking for noise suppression to run. A request,
    /// not a setting: the node answers with what it will actually do.
    pub suppress: voice_dsp::Mode,
    /// Proof the caller may be in this room. `None` is the loopback demo.
    pub ticket: Option<&'a str>,
}

impl Join<'_> {
    /// The query string, without the leading `?`.
    pub fn query(&self) -> String {
        let mut query = format!(
            "room={}&id={}&suppress={}",
            self.room,
            self.id,
            self.suppress.as_str(),
        );
        if let Some(ticket) = self.ticket {
            query.push_str("&token=");
            query.push_str(ticket);
        }
        query
    }
}

/// One thing arriving from the relay.
pub enum Incoming {
    /// A voice packet, from some other participant.
    Binary(Vec<u8>),
    /// A notice: a redirect, a duplicate id, what suppression the room is
    /// getting. Never audio.
    Text(String),
    /// The connection is gone.
    Closed,
}

/// The publish half — the one the capture callback holds.
pub enum Publish {
    Socket(WsSender),
    #[cfg(target_arch = "wasm32")]
    Datagram(quic::Datagrams),
}

impl Publish {
    /// Send one voice packet. `false` means it did not go.
    ///
    /// Synchronous on both carriers because the caller is: this runs inside
    /// `AudioStream::subscribe`, on the audio thread on native and the main
    /// thread on web, and neither of those can await.
    pub fn binary(&mut self, bytes: Vec<u8>) -> bool {
        match self {
            Publish::Socket(sender) => {
                let len = bytes.len() as u32;
                match sender.send(WsMessage::Binary(bytes)) {
                    Ok(()) => {
                        crate::session::record_sent(len);
                        true
                    }
                    // The socket closed. The receive half will notice and say
                    // so; counting is all that happens on the hot path.
                    Err(_) => {
                        crate::session::record_unsent();
                        false
                    }
                }
            }
            #[cfg(target_arch = "wasm32")]
            Publish::Datagram(datagrams) => datagrams.queue(bytes),
        }
    }

    /// Hang up.
    pub fn close(&mut self) {
        match self {
            Publish::Socket(sender) => sender.close(),
            #[cfg(target_arch = "wasm32")]
            Publish::Datagram(datagrams) => datagrams.close(),
        }
    }

    /// The one word that goes in a log line or a stats probe.
    pub fn name(&self) -> &'static str {
        match self {
            Publish::Socket(_) => "websocket",
            #[cfg(target_arch = "wasm32")]
            Publish::Datagram(_) => "webtransport",
        }
    }
}

/// The receive half — owned by the task that drives the connection.
pub enum Receive {
    Socket(WebSocket),
    #[cfg(target_arch = "wasm32")]
    Datagram(datagram::Link),
}

impl Receive {
    /// Wait for whatever comes next.
    pub async fn next(&mut self) -> Incoming {
        match self {
            Receive::Socket(socket) => match socket.recv().await {
                Some(Ok(WsMessage::Binary(bytes))) => Incoming::Binary(bytes),
                Some(Ok(WsMessage::Text(text))) => Incoming::Text(text),
                // Ping, pong and close never surface from this SDK, so `None`
                // and an error are the only two ways this ends.
                None | Some(Err(_)) => Incoming::Closed,
            },
            #[cfg(target_arch = "wasm32")]
            Receive::Datagram(link) => match link.recv().await {
                datagram::Event::Data(bytes) => Incoming::Binary(bytes),
                datagram::Event::Control(text) => Incoming::Text(text),
                datagram::Event::Closed => Incoming::Closed,
            },
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use quic::datagrams;

#[cfg(target_arch = "wasm32")]
mod quic {
    use futures::channel::mpsc;
    use futures::StreamExt;

    use super::Publish;

    /// How many packets may be waiting for the send task at once.
    ///
    /// Ten, which is 200 ms. Less a tuning knob than a statement about what
    /// this queue is for: it crosses the sync/async boundary and nothing else.
    /// A send path that has fallen a fifth of a second behind will not catch up
    /// by being given more room to fall further.
    const QUEUE: usize = 10;

    /// The write end of a QUIC link, made callable from a synchronous callback.
    ///
    /// Holds no link of its own — the draining task owns that. So dropping this
    /// (which is what a redirect does, when it swaps the publish half) ends the
    /// task and closes the session, with no second owner to remember.
    pub struct Datagrams {
        packets: mpsc::Sender<Vec<u8>>,
    }

    impl Datagrams {
        pub(super) fn queue(&mut self, bytes: Vec<u8>) -> bool {
            match self.packets.try_send(bytes) {
                Ok(()) => true,
                // Full, or the task is gone. Either way this packet is not
                // going to be heard, and pretending otherwise would make a
                // stalled send path look like a quiet room.
                Err(_) => {
                    crate::session::record_unsent();
                    false
                }
            }
        }

        pub(super) fn close(&mut self) {
            self.packets.close_channel();
        }
    }

    /// Publish over `link`, draining the queue on a task of its own.
    pub fn datagrams(link: datagram::Link) -> Publish {
        let (packets, mut queued) = mpsc::channel::<Vec<u8>>(QUEUE);
        runtime_core::driver::spawn_async(async move {
            // Reported once. A stream that cannot fit a datagram cannot fit the
            // next one either, so the second line onwards would be noise
            // burying the first.
            let mut oversize = 0u32;
            while let Some(bytes) = queued.next().await {
                let len = bytes.len() as u32;
                match link.send(&bytes).await {
                    // Handed to the network. Counted HERE rather than at the
                    // queue, so `sent` means what it says on both carriers.
                    Ok(()) => crate::session::record_sent(len),
                    Err(datagram::LinkError::TooLarge { size, limit }) => {
                        crate::session::record_unsent();
                        oversize += 1;
                        if oversize == 1 {
                            crate::session::web_log(&format!(
                                "[voice] {size} bytes will not fit a {limit}-byte datagram — \
                                 this call is not encoding its audio, so none of it is being \
                                 heard. See voice_proto::codec::Encoded::note.",
                            ));
                        }
                    }
                    Err(e) => {
                        crate::session::record_unsent();
                        crate::session::web_log(&format!("[voice] link closed while sending: {e}"));
                        break;
                    }
                }
            }
            // The queue is done with, so the session is too — either the call
            // ended or a redirect replaced this half.
            link.close(0, "bye");
        });
        Publish::Datagram(Datagrams { packets })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_join_names_the_room_the_id_and_what_it_wants() {
        let join = Join {
            room: "porch",
            id: 42,
            suppress: voice_dsp::Mode::Client,
            ticket: Some("abc"),
        };
        assert_eq!(join.query(), "room=porch&id=42&suppress=client&token=abc");
    }

    #[test]
    fn a_join_with_no_ticket_carries_no_token() {
        // The loopback demo. A `token=` with nothing after it is not the same
        // thing, and a node that checks tickets would refuse it with a message
        // about a malformed one rather than a missing one.
        let join = Join { room: "lobby", id: 1, suppress: voice_dsp::Mode::Off, ticket: None };
        assert_eq!(join.query(), "room=lobby&id=1&suppress=off");
    }
}
