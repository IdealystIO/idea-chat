//! Keeping the live connection up.
//!
//! The framework's socket hook connects when mounted and stays closed once
//! it drops, so reconnecting is re-mounting it: the socket lives under a
//! `match` keyed on its ticket, and a fresh ticket — fetched after a
//! backoff — is a fresh socket. A ticket is good for a minute and only for
//! opening, so every attempt fetches its own.
//!
//! The retries are bound to [`LiveLink`]'s life, not to whatever scope is
//! ambient when they are scheduled. A failed ticket is answered from an
//! async callback, which has no reactive scope at all — `after_ms_scoped`
//! there is a silent no-op — and a restart is scheduled from inside the
//! dying socket's status effect, whose next run cancels what it scheduled.
//! Either way one outage longer than an attempt stopped reconnection for
//! good, until a reload.

use std::cell::Cell;
use std::rc::Rc;

use chat_model::Id;
use runtime_core::{component, effect, signal, ui, Element, ScopeAlive, Signal};
use server::SocketStatus;

use crate::platform;
use crate::store::{store, Conn};

/// How often an open connection says it is still there.
const PING_MS: i32 = 25_000;
/// The longest wait between reconnection attempts.
const BACKOFF_MAX_MS: i32 = 15_000;

/// Fetch a ticket, now or after `delay_ms`, and hand it over — for as long
/// as `alive` (the [`LiveLink`]) is mounted.
fn fetch_ticket(alive: ScopeAlive, device: Option<Id>, delay_ms: i32, ticket: Signal<Option<(String, u64)>>, attempt: u64) {
    let go = move || {
        if !alive.get() {
            return;
        }
        let again = alive.clone();
        runtime_core::spawn_then_in(&alive, chat_api::auth::live_ticket(device), move |got| match got {
            Ok(t) => ticket.set(Some((t, attempt))),
            Err(e) if chat_api::is_signed_out(&e) => crate::app::signed_out(),
            // The server is unreachable: try again, later.
            Err(_) => fetch_ticket(again, device, (delay_ms.max(1_000) * 2).min(BACKOFF_MAX_MS), ticket, attempt + 1),
        });
    };
    if delay_ms <= 0 {
        go();
    } else {
        runtime_core::after_ms_detached(delay_ms, go);
    }
}

/// The live connection, as an invisible element under the signed-in root.
#[component]
pub fn LiveLink(#[prop(static)] device: Option<Id>) -> Element {
    let ticket: Signal<Option<(String, u64)>> = signal(None);
    let failures = Rc::new(Cell::new(0u32));
    let alive = ScopeAlive::current();
    fetch_ticket(alive.clone(), device, 0, ticket, 0);
    let restart: Rc<dyn Fn(u64)> = {
        let failures = failures.clone();
        Rc::new(move |attempt: u64| {
            store().detach();
            let n = failures.get() + 1;
            failures.set(n);
            let delay = (500 * 2i32.saturating_pow(n.min(5))).min(BACKOFF_MAX_MS);
            fetch_ticket(alive.clone(), device, delay, ticket, attempt + 1);
        })
    };

    // Ticking the coarse clock keeps "typing…" and relative times honest.
    fn tick_forever() {
        runtime_core::after_ms_scoped(2_000, || {
            store().tick();
            tick_forever();
        });
    }
    tick_forever();

    ui! {
        match ticket.get() {
            Some((t, attempt)) => {
                Socket(ticket = t.clone(), attempt = *attempt, restart = restart.clone(), failures = failures.clone())
            }
            None => { runtime_core::fragment(Vec::new()) }
        }
    }
}

#[component]
fn Socket(
    #[prop(static)] ticket: String,
    #[prop(static)] attempt: u64,
    #[prop(static, default = Rc::new(|_| {}) as Rc<dyn Fn(u64)>)] restart: Rc<dyn Fn(u64)>,
    #[prop(static, default = Rc::new(Cell::new(0)))] failures: Rc<Cell<u32>>,
) -> Element {
    let s = store();
    let sock = chat_api::live::live(ticket);
    let done = Rc::new(Cell::new(false));
    {
        let (s, restart, done) = (s.clone(), restart.clone(), done.clone());
        effect!({
            if let Some(down) = sock.incoming().get() {
                if !s.receive(down) && !done.replace(true) {
                    // Missed an event: start over with a fresh snapshot.
                    restart(attempt);
                }
            }
        });
    }
    {
        let (s, done) = (s.clone(), done.clone());
        effect!({
            match sock.status() {
                SocketStatus::Open => {
                    failures.set(0);
                    s.attach(Rc::new(move |up| sock.send(up)));
                    keep_alive(done.clone());
                }
                SocketStatus::Closed | SocketStatus::Error => {
                    if !done.replace(true) {
                        restart(attempt);
                    }
                }
                SocketStatus::Connecting => {}
            }
        });
    }
    let _ = platform::visible;
    let _ = Conn::Open;
    runtime_core::fragment(Vec::new())
}

fn keep_alive(done: Rc<Cell<bool>>) {
    runtime_core::after_ms_scoped(PING_MS, move || {
        if !done.get() {
            store().ping();
            keep_alive(done);
        }
    });
}
