//! Idea Chat, the app: one idealyst tree for the web, desktop and phones.
//!
//! - [`app`] — the root: theme, server, whether anyone is signed in.
//! - [`auth`] — the signed-out screens.
//! - [`shell`] — the signed-in layout and navigation.
//! - [`store`] — the client's state; [`live`] keeps it in step.
//! - [`nav`], [`home`], [`channel`], [`overlays`] — what is on screen.
//! - [`bits`], [`forms`] — what the screens share.

mod app;
mod auth;
mod bits;
mod calls;
mod channel;
mod composer;
mod context;
mod device;
mod forms;
mod home;
mod live;
mod media;
mod message;
mod nav;
mod overlays;
mod platform;
mod settings;
mod shell;
mod sounds;
mod store;
mod theme;
mod uploads;

use std::cell::Cell;

use runtime_core::{Element, Signal};

thread_local! {
    /// The theme switch, so the signed-in UI can flip it.
    pub(crate) static DARK: Cell<Option<Signal<bool>>> = const { Cell::new(None) };
}

/// Scene extensions: video tiles for calls (web), nothing elsewhere.
pub fn register_scene_extensions<H: idealyst::SceneHost>(registry: &mut runtime_scene::Registry<H>) {
    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    linux_executor::install();
    call_video::register_scene_extensions(registry);
}

/// The GTK host installs no async executor (host-gtk 3.0.0), so
/// `runtime_core::driver::spawn_async` falls back to `pollster::block_on`
/// on the calling thread — the UI thread. A future that never finishes,
/// like the live connection, then freezes the window for good, right after
/// sign-in. This runs futures on GLib's main loop instead, between events,
/// as the web backend runs them on the JS event loop. Installed from the
/// first hook the host calls on the main thread, before the app is built.
///
/// Remove once the GTK host installs one itself.
#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
mod linux_executor {
    use std::future::Future;
    use std::pin::Pin;

    struct Glib;

    impl runtime_core::driver::AsyncExecutor for Glib {
        fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + 'static>>) {
            let ctx = glib::MainContext::default();
            if ctx.is_owner() {
                ctx.spawn_local(future);
            } else {
                // Off the main thread there is no loop of ours: as before.
                pollster::block_on(future);
            }
        }
    }

    pub fn install() {
        // Own the default context on this, the main, thread from now on,
        // so what is spawned while the app is built is queued rather than
        // refused; GTK's loop acquires it again, which is recursive.
        let ctx = glib::MainContext::default();
        if let Ok(guard) = ctx.acquire() {
            std::mem::forget(guard);
        }
        runtime_core::driver::install_async_executor(Box::new(Glib));
    }
}

pub fn scene_app() -> Element {
    app()
}

pub fn app() -> Element {
    runtime_core::ui! { app::App() }
}
