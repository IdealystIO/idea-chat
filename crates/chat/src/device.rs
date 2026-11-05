//! This device's key: made once, kept in the platform's secret store, and
//! published (its public half) so calls can be sealed to it.

use std::cell::RefCell;

use chat_keys::DeviceKey;
use chat_model::Id;
use runtime_core::Signal;

use crate::platform;

thread_local! {
    static DEVICE: RefCell<Option<(Id, DeviceKey)>> = const { RefCell::new(None) };
}

/// This device's id and key, once registered.
pub fn current() -> Option<(Id, DeviceKey)> {
    DEVICE.with(|d| d.borrow().clone())
}

fn key() -> DeviceKey {
    if let Some(k) = platform::secret("device-key").and_then(|t| DeviceKey::from_text(&t).ok()) {
        return k;
    }
    let k = DeviceKey::generate(platform::random::<32>());
    platform::set_secret("device-key", &k.to_text());
    k
}

/// Register this device and say so on `done`: `Some(Some(id))` when it is,
/// `Some(None)` when it could not be (the app works; calls will not).
pub fn ensure(done: Signal<Option<Option<Id>>>) {
    let key = key();
    let public = key.public().as_text();
    runtime_core::spawn_then(chat_api::auth::register_device(platform::device_name(), public), move |got| match got {
        Ok(device) => {
            DEVICE.with(|d| *d.borrow_mut() = Some((device.id, key)));
            done.set(Some(Some(device.id)));
        }
        Err(e) if chat_api::is_signed_out(&e) => crate::app::signed_out(),
        Err(e) => {
            eprintln!("[chat] device registration failed: {}", chat_api::describe(&e));
            done.set(Some(None));
        }
    });
}
