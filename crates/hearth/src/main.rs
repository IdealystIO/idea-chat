//! Entry point. The macro wires the platform shell for whatever target this
//! is built for and mounts `hearth::app()`.

#[cfg(not(feature = "server"))]
idealyst::entry!(hearth);

/// Under `--features server` there is no UI in this crate to mount — the
/// artifact is `bin/server.rs`. This target still has to produce a `main`, so
/// it produces one that says where to look rather than failing to build.
#[cfg(feature = "server")]
fn main() {
    eprintln!("This is the client entry point, and the client is not built with");
    eprintln!("--features server. To run the server:");
    eprintln!("  cargo run -p hearth --bin server --features server");
    std::process::exit(2);
}
