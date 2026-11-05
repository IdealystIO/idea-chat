//! What differs by platform, in one place: the clock, randomness, where the
//! server is, how the session and the device key are kept, and (on the web)
//! the address bar.

use std::sync::{Arc, OnceLock};

/// Wall-clock milliseconds since the epoch.
pub fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }
}

/// Minutes east of UTC, for drawing local days and times.
pub fn utc_offset_min() -> i32 {
    #[cfg(target_arch = "wasm32")]
    {
        -(js_sys::Date::new_0().get_timezone_offset() as i32)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

/// `N` bytes from the platform's CSPRNG.
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::getrandom(&mut out).expect("the platform has a CSPRNG");
    out
}

/// Where the server is. On the web, wherever the page came from; natively,
/// `IDEA_CHAT_SERVER` at build time, else a local server.
pub fn server_url() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        deep_link::origin().unwrap_or_else(|| "http://localhost:8090".into())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        option_env!("IDEA_CHAT_SERVER").unwrap_or("http://localhost:8090").to_string()
    }
}

/// An absolute URL for a path the server serves (`/f/<key>`), as images and
/// links need on every platform.
pub fn absolute(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{}{}", server_url().trim_end_matches('/'), path)
    }
}

fn prefs() -> &'static Arc<dyn storage::Storage> {
    static STORE: OnceLock<Arc<dyn storage::Storage>> = OnceLock::new();
    STORE.get_or_init(|| storage::platform_storage("idea-chat"))
}

/// A preference: theme, last channel per space. Plaintext.
pub fn pref(key: &str) -> Option<String> {
    prefs().get_now(key).ok().flatten()
}

pub fn set_pref(key: &str, value: &str) {
    let _ = prefs().set_now(key, value);
}

pub fn clear_pref(key: &str) {
    let _ = prefs().remove_now(key);
}

/// A secret: the native session token, the device key. On native, the OS
/// keystore. On the web there is none to use — the session is an httpOnly
/// cookie, and the device key goes in the page's own storage, where only
/// this origin's script can read it.
pub fn secret(key: &str) -> Option<String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        credentials::platform_credentials("dev.ideachat.app").get(key).ok().flatten()
    }
    #[cfg(target_arch = "wasm32")]
    {
        pref(&format!("secret.{key}"))
    }
}

pub fn set_secret(key: &str, value: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = credentials::platform_credentials("dev.ideachat.app").set(key, value);
    }
    #[cfg(target_arch = "wasm32")]
    {
        set_pref(&format!("secret.{key}"), value);
    }
}

pub fn clear_secret(key: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = credentials::platform_credentials("dev.ideachat.app").remove(key);
    }
    #[cfg(target_arch = "wasm32")]
    {
        clear_pref(&format!("secret.{key}"));
    }
}

/// Point the API client at the server. Native clients send their bearer
/// token and say they are native, so sign-in answers with a token rather
/// than a cookie.
pub fn configure_server() {
    let config = server::ClientConfig::new(server_url());
    #[cfg(not(target_arch = "wasm32"))]
    let config = config.with_credentials(server::credentials_from_fn(|| {
        let mut headers = vec![("x-idealyst-client".to_string(), "native".to_string())];
        if let Some(token) = secret("session") {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        headers
    }));
    server::configure(config);
}

/// A name for this device, for the account's device list.
pub fn device_name() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let agent = web_sys::window().and_then(|w| w.navigator().user_agent().ok()).unwrap_or_default();
        let browser = if agent.contains("Firefox") {
            "Firefox"
        } else if agent.contains("Edg/") {
            "Edge"
        } else if agent.contains("Chrome") {
            "Chrome"
        } else if agent.contains("Safari") {
            "Safari"
        } else {
            "Browser"
        };
        let os = if agent.contains("Mac OS") {
            "macOS"
        } else if agent.contains("Windows") {
            "Windows"
        } else if agent.contains("Android") {
            "Android"
        } else if agent.contains("iPhone") || agent.contains("iPad") {
            "iOS"
        } else if agent.contains("Linux") {
            "Linux"
        } else {
            ""
        };
        if os.is_empty() { browser.to_string() } else { format!("{browser} on {os}") }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        format!("Idea Chat on {}", std::env::consts::OS)
    }
}

/// The page's path and query at start — for links into the app (a verify
/// link, an invite). Natively there is none yet.
pub fn entry_link() -> Option<(String, Vec<(String, String)>)> {
    #[cfg(target_arch = "wasm32")]
    {
        let location = web_sys::window()?.location();
        let path = location.pathname().ok()?;
        let search = location.search().ok().unwrap_or_default();
        let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
        let mut query = Vec::new();
        for key in ["email", "code"] {
            if let Some(v) = params.get(key) {
                query.push((key.to_string(), v));
            }
        }
        Some((path, query))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

/// Put the address bar back to `/` without a reload, once a link into the
/// app has been used (so a reload doesn't use it again).
pub fn clear_entry_link() {
    #[cfg(target_arch = "wasm32")]
    if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some("/"));
    }
}

/// Whether the page is in front of the person.
pub fn visible() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.document())
            .is_none_or(|d| d.visibility_state() == web_sys::VisibilityState::Visible)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        true
    }
}
