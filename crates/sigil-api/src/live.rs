//! The live connection: one WebSocket per open device, on which the server
//! pushes what the device would otherwise poll for.
//!
//! ```text
//! live_ticket() ──▶ ticket          (a session-authenticated call)
//! sigil_live(ticket) ── open ──▶    ◀── Ready
//!                                   ◀── Commit / Welcome / Message / Record …
//! Up::Ping ──▶                      ◀── Pong
//! ```
//!
//! `Record` only says a record arrived; fetch it with `records`. Everything
//! else carries its bytes, which are ciphertext or public.

use serde::{Deserialize, Serialize};
use server::ServerError;

use crate::ApiError;

/// Server to device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Down {
    Ready,
    Refused { reason: String },
    Pong,
    /// A commit another device made in a group this device is in.
    Commit { group: String, commit: String },
    /// This device was added to a group.
    Welcome { welcome: String },
    /// A reliable realtime message.
    Message { group: String, message: String },
    /// A record was stored.
    Record { group: String, service: u16, seq: u64 },
}

/// Device to server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Up {
    Ping,
}

#[server::channel]
pub async fn sigil_live(mut ch: server::Socket<Up, Down>, ticket: String) -> Result<(), ServerError<ApiError>> {
    use crate::srv::{app, check_ticket, now_ms};

    let a = app();
    let Some(device) = check_ticket(&a.live_secret, &ticket, now_ms()) else {
        let _ = ch.send(Down::Refused { reason: "signed out".into() }).await;
        return Ok(());
    };
    let (conn, mut pushes) = a.router.register(device.clone());
    let mut open = ch.send(Down::Ready).await.is_ok();
    while open {
        tokio::select! {
            up = ch.recv() => match up {
                Some(Ok(Up::Ping)) => open = ch.send(Down::Pong).await.is_ok(),
                Some(Err(_)) | None => open = false,
            },
            push = pushes.recv() => match push {
                Some(down) => open = ch.send(down).await.is_ok(),
                None => open = false,
            },
        }
    }
    a.router.unregister(&device, conn);
    Ok(())
}
