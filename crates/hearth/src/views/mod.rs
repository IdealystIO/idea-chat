//! The views. One module per region of the shell.

pub mod auth;
pub mod bits;
pub mod chat;
pub mod live;
pub mod overlays;
pub mod person_menu;
pub mod rail;
pub mod rooms;
pub mod settings;
#[cfg(target_arch = "wasm32")]
pub mod suppressor;
pub mod thread;
