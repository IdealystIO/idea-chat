//! Entry point. The macro wires the platform shell for whatever target this is
//! built for and mounts `voice_client::app()`; picking a platform is a build
//! concern (target triple + features), never a code one.
idealyst::entry!(voice_client);
