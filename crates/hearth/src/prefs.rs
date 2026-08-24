//! Preferences that belong to this device rather than to this account.
//!
//! Nothing survived a refresh before this: every setting was an in-memory
//! signal that reset to its default on load. Nobody minds re-picking a theme
//! once; being asked to turn noise suppression back on every time you open the
//! app is a different thing, because the reason you turned it on is still true.
//!
//! # Why local, and not on the server
//!
//! These are **device** facts, not account facts. Whether to spend a machine's
//! CPU on a neural model depends on that machine; how hard to chase keystrokes
//! depends on that keyboard; a theme depends on the screen you are looking at.
//! Syncing them would mean a phone inheriting a desktop's decision about the
//! desktop's microphone.
//!
//! Preferences that genuinely belong to the account — who may notify you, read
//! receipts — are not here, and should be server-side when they are done
//! properly. Storing half of them locally would be worse than storing none.
//!
//! # Failure is not an error
//!
//! Every read returns the default and every write is dropped when storage is
//! unavailable — a private window, a browser with site data disabled, a quota
//! that is full. A settings page that refused to work because it could not
//! remember your choice would be a worse product than one that forgets.

/// Read one preference, or `None` if it was never stored or storage is unusable.
pub fn get(key: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()?.local_storage().ok()??.get_item(&full(key)).ok()?
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = key;
        None
    }
}

/// Store one preference. Silently does nothing if storage is unusable.
pub fn set(key: &str, value: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(store) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
            let _ = store.set_item(&full(key), value);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (key, value);
    }
}

/// Namespaced, because an artifact origin is shared with whatever else the
/// operator serves from it.
#[cfg(target_arch = "wasm32")]
fn full(key: &str) -> String {
    format!("hearth.{key}")
}

/// Read a preference through a parser, falling back to `default`.
pub fn parsed<T>(key: &str, parse: impl Fn(&str) -> T, default: T) -> T {
    match get(key) {
        Some(raw) => parse(&raw),
        None => default,
    }
}

/// Read a boolean preference.
pub fn flag(key: &str, default: bool) -> bool {
    parsed(key, |v| v == "on", default)
}

/// Store a boolean preference.
pub fn set_flag(key: &str, value: bool) {
    set(key, if value { "on" } else { "off" });
}
