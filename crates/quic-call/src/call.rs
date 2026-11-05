//! The call: `call_session`, with what this page adds — where the
//! DeepFilterNet model is, and reading the page's own address (the relay and
//! room in its query, the end-to-end key in its fragment).

pub use call_audio::{ModelSettings, Processing, Suppression, VoiceStages};
pub use call_session::{
    log, reprocess, retune, set_muted,  set_voice_processing, set_volume, sync_screen_audio,
    Access, Call, Renew, Traffic,
};

/// Where the DeepFilterNet model is served from: the bundle's `assets/`, which
/// `idealyst build` copies from `crates/quic-call/assets/`. Not compiled into
/// the wasm — 8 MB every visitor would download whether or not they use it.
const MODEL_PATH: &str = "/assets/DeepFilterNet3_onnx.tar.gz";

/// Dial and start the call; see `call_session::join`.
pub async fn join(
    url: &str,
    cert_hash: &str,
    access: Access,
    processing: Processing,
    voice: VoiceStages,
    model: ModelSettings,
    on_control: impl Fn(String) + 'static,
) -> Result<Call, String> {
    let settings = call_session::Settings {
        processing,
        voice,
        model,
        model_path: MODEL_PATH,
        // This page keeps volumes per tile, by relay number.
        gains: Default::default(),
        default_gain: 1.0,
    };
    call_session::join(url, cert_hash, access, settings, on_control).await
}

/// Where to dial when the address does not say: the page's own host on 4433,
/// when the page came over https — which is the deployment where the same
/// certificate serves both.
pub fn default_relay() -> Option<String> {
    let location = web_sys::window()?.location();
    if location.protocol().ok()? != "https:" {
        return None;
    }
    Some(format!("https://{}:4433/", location.hostname().ok()?))
}

/// `?url=…&hash=…` from the page address, which is how the relay's printed link
/// fills the form in.
pub fn query(key: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search).ok()?.get(key)
}

/// The end-to-end key from the address's `#key=…`. The fragment never leaves
/// the browser, so a link carries the key to people without the relay — or
/// any server — seeing it.
pub fn key_from_address() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    let raw = hash.strip_prefix('#')?.split('&').find_map(|kv| kv.strip_prefix("key="))?;
    js_sys::decode_uri_component(raw).ok().map(String::from).filter(|k| !k.trim().is_empty())
}

/// Put the key in the address (or take it out, for none), so the address bar
/// is the link to share. Replaces the history entry rather than adding one.
pub fn show_key_in_address(key: &str) {
    let Some(window) = web_sys::window() else { return };
    let location = window.location();
    let (Ok(path), Ok(search)) = (location.pathname(), location.search()) else { return };
    let key = key.trim();
    let url = if key.is_empty() {
        format!("{path}{search}")
    } else {
        format!("{path}{search}#key={}", String::from(js_sys::encode_uri_component(key)))
    };
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&wasm_bindgen_futures::wasm_bindgen::JsValue::NULL, "", Some(&url));
    }
}

/// A fresh end-to-end key: 128 bits from the browser's CSPRNG.
pub fn new_key() -> Option<String> {
    let mut random = [0u8; 16];
    web_sys::window()?.crypto().ok()?.get_random_values_with_u8_array(&mut random).ok()?;
    Some(call_e2ee::RoomKey::generate(random).1)
}
