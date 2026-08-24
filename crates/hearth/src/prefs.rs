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

/// Every stored key beginning with `prefix`, with the prefix stripped, paired
/// with its value.
///
/// One caller: per-person volume. The alternative is asking the server who is
/// in the room and looking each of them up, which answers a different question
/// — a volume set for someone who has not spoken yet still has to be in force
/// the moment they do, and the only record that they were ever turned down is
/// this store.
pub fn with_prefix(prefix: &str) -> Vec<(String, String)> {
    #[cfg(target_arch = "wasm32")]
    {
        let Some(store) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() else {
            return Vec::new();
        };
        let full_prefix = full(prefix);
        let count = store.length().unwrap_or(0);
        (0..count)
            .filter_map(|i| store.key(i).ok().flatten())
            .filter_map(|key| {
                let rest = key.strip_prefix(&full_prefix)?.to_string();
                let value = store.get_item(&key).ok().flatten()?;
                Some((rest, value))
            })
            .collect()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = prefix;
        Vec::new()
    }
}

/// Forget one preference, so it falls back to its default rather than being
/// stored as one. Keeps the store from accumulating a row per person the user
/// once nudged and set back.
pub fn clear(key: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(store) = web_sys::window().and_then(|w| w.local_storage().ok()).flatten() {
            let _ = store.remove_item(&full(key));
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = key;
    }
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

/// Read a number, falling back to `default` if it was never stored or does not
/// parse. A corrupt value is a default, not a panic: this store is editable by
/// anyone with a devtools console.
pub fn number(key: &str, default: f32) -> f32 {
    match get(key).and_then(|raw| raw.parse::<f32>().ok()) {
        Some(v) if v.is_finite() => v,
        _ => default,
    }
}

/// Store a number.
pub fn set_number(key: &str, value: f32) {
    set(key, &format!("{value}"));
}
