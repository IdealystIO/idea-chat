//! Room tickets: proof that a particular person may join a particular room.
//!
//! # Why this exists
//!
//! A relay node's job is to forward bytes, and it has no idea what a space or
//! a membership is. Left alone it will admit anyone who connects with a room
//! name — which is fine for a two-tab loopback demo and completely wrong for a
//! chat server, where the room name is a channel id that anybody can read off
//! their own client and guess at for other channels.
//!
//! The chat server already knows who you are and what you can see. So it says
//! so, in a form the node can check on its own:
//!
//! ```text
//!   client ── "let me into room R" ──▶ chat server   (session + membership check)
//!          ◀── ticket for (R, me, +2min) ──
//!          ── connect ws?room=R&token=… ──▶ relay node   (verify, no network)
//! ```
//!
//! An HMAC rather than a lookup because the alternative is a node calling back
//! into the chat server on every connection — a hard dependency in the wrong
//! direction, and one more thing to be down when someone is trying to talk.
//! The node needs no database, no cache, and no route to the chat server; it
//! needs the shared secret and a clock.
//!
//! # What a ticket does NOT do
//!
//! It is not a session and it is not a capability for anything but this one
//! room. It cannot be traded for chat access, it names no permissions beyond
//! "may be present", and it expires in minutes — short enough that revoking
//! membership takes effect on the next join rather than never, long enough to
//! survive a slow reconnect.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// How long a freshly minted ticket is good for.
///
/// Long enough to cover a user tapping "join" and a socket actually opening
/// over a bad connection; short enough that someone removed from a space stops
/// being able to open NEW connections quickly. It deliberately does not govern
/// how long you may stay — that is the node's business, and cutting a live call
/// because a token aged out would be its own bug.
pub const TICKET_TTL_SECS: u64 = 120;

/// Why a ticket was refused. Deliberately coarse on the wire — the client is
/// told to try again, not which byte was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    /// Not `exp.user.signature`.
    Malformed,
    /// Signature did not match: wrong secret, wrong room, or forged.
    BadSignature,
    /// `exp` is in the past.
    Expired,
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TicketError::Malformed => "malformed ticket",
            TicketError::BadSignature => "ticket is not valid for this room",
            TicketError::Expired => "ticket has expired",
        })
    }
}

impl std::error::Error for TicketError {}

/// Who a verified ticket says you are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    /// The chat server's id for the person. Opaque to the node.
    pub user: String,
    /// Unix seconds after which this ticket is no good.
    pub expires_at: u64,
}

/// The signed bytes.
///
/// `room` is part of the signature but NOT part of the token, because the node
/// already has it from the connection's own query string. Signing over it is
/// what stops a ticket for a room you can see being replayed against one you
/// cannot. The separator is a NUL so that no combination of room and user can
/// be re-split into a different pair.
fn message(room: &str, user: &str, expires_at: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(room.len() + user.len() + 24);
    m.extend_from_slice(room.as_bytes());
    m.push(0);
    m.extend_from_slice(user.as_bytes());
    m.push(0);
    m.extend_from_slice(expires_at.to_string().as_bytes());
    m
}

fn sign(secret: &[u8], room: &str, user: &str, expires_at: u64) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac takes a key of any length");
    mac.update(&message(room, user, expires_at));
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Mint a ticket letting `user` into `room` until `now + TICKET_TTL_SECS`.
///
/// `now` is passed in rather than read here so this stays usable from a wasm
/// build and testable without waiting.
pub fn mint(secret: &[u8], room: &str, user: &str, now: u64) -> String {
    let expires_at = now + TICKET_TTL_SECS;
    format!("{expires_at}.{user}.{}", sign(secret, room, user, expires_at))
}

/// Check a ticket against the room it is being presented for.
///
/// Order matters: the signature is checked before the clock. Reporting
/// "expired" for a token we cannot even authenticate would tell a forger that
/// their structure was right and only the time was wrong.
pub fn verify(secret: &[u8], room: &str, token: &str, now: u64) -> Result<Ticket, TicketError> {
    // The user id is opaque and could in principle contain a dot, so split off
    // the two fixed ends rather than the middle.
    let (exp_str, rest) = token.split_once('.').ok_or(TicketError::Malformed)?;
    let (user, sig) = rest.rsplit_once('.').ok_or(TicketError::Malformed)?;
    if user.is_empty() || sig.is_empty() {
        return Err(TicketError::Malformed);
    }
    let expires_at: u64 = exp_str.parse().map_err(|_| TicketError::Malformed)?;

    let expected = sign(secret, room, user, expires_at);
    if !constant_time_eq(expected.as_bytes(), sig.as_bytes()) {
        return Err(TicketError::BadSignature);
    }
    if now >= expires_at {
        return Err(TicketError::Expired);
    }
    Ok(Ticket { user: user.to_string(), expires_at })
}

/// Compare without leaking where the first difference is.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &[u8] = b"a shared secret";
    const NOW: u64 = 1_700_000_000;

    #[test]
    fn a_freshly_minted_ticket_verifies_for_its_own_room() {
        let t = mint(S, "room-a", "user-1", NOW);
        let got = verify(S, "room-a", &t, NOW).expect("valid");
        assert_eq!(got.user, "user-1");
        assert_eq!(got.expires_at, NOW + TICKET_TTL_SECS);
    }

    /// The whole point: a ticket for a room you belong to must not open a room
    /// you do not. This is what a plain "is this token signed by us" check
    /// would miss.
    #[test]
    fn a_ticket_does_not_work_on_another_room() {
        let t = mint(S, "room-a", "user-1", NOW);
        assert_eq!(verify(S, "room-b", &t, NOW), Err(TicketError::BadSignature));
    }

    #[test]
    fn another_deployments_secret_does_not_open_this_one() {
        let t = mint(b"someone elses secret", "room-a", "user-1", NOW);
        assert_eq!(verify(S, "room-a", &t, NOW), Err(TicketError::BadSignature));
    }

    #[test]
    fn a_ticket_stops_working_once_it_expires() {
        let t = mint(S, "room-a", "user-1", NOW);
        assert!(verify(S, "room-a", &t, NOW + TICKET_TTL_SECS - 1).is_ok());
        assert_eq!(verify(S, "room-a", &t, NOW + TICKET_TTL_SECS), Err(TicketError::Expired));
    }

    /// Moving the expiry to buy more time invalidates the signature it was
    /// signed with — the deadline is not advisory.
    #[test]
    fn the_expiry_cannot_be_edited_forward() {
        let t = mint(S, "room-a", "user-1", NOW);
        let (_, rest) = t.split_once('.').unwrap();
        let forged = format!("{}.{}", NOW + 999_999, rest);
        assert_eq!(verify(S, "room-a", &forged, NOW), Err(TicketError::BadSignature));
    }

    #[test]
    fn claiming_to_be_someone_else_invalidates_it() {
        let t = mint(S, "room-a", "user-1", NOW);
        let forged = t.replace("user-1", "user-2");
        assert_eq!(verify(S, "room-a", &forged, NOW), Err(TicketError::BadSignature));
    }

    #[test]
    fn nonsense_is_refused_without_panicking() {
        for junk in ["", ".", "..", "abc", "1.2", "x.user.sig", &"9".repeat(40)] {
            assert!(verify(S, "room-a", junk, NOW).is_err(), "accepted {junk:?}");
        }
    }

    /// A user id containing the separator must still round-trip, or the parse
    /// would silently attribute the connection to a truncated name.
    #[test]
    fn a_user_id_with_a_dot_still_round_trips() {
        let t = mint(S, "room-a", "first.last", NOW);
        assert_eq!(verify(S, "room-a", &t, NOW).unwrap().user, "first.last");
    }
}
